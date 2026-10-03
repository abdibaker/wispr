#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod audio;
mod history;
mod hotkey;
mod insertion;
mod logger;
mod providers;
mod settings;

use anyhow::{anyhow, Result};
use history::{Entry, HistoryStore};
use hotkey::{HotkeyEvent, HotkeyManager};
use providers::{OpenAiCompatible, PromptCleaner, SpeechProvider};
use serde::Serialize;
use settings::Settings;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

#[derive(Serialize, Clone, Default)]
struct Latency {
    hotkey_to_recording_ms: u64,
    release_to_stt_ms: u64,
    stt_to_cleanup_ms: u64,
    cleanup_to_insert_ms: u64,
    release_to_prompt_ms: u64,
    audio_ms: u64,
}

struct AppState {
    settings: Mutex<Settings>,
    history: Mutex<HistoryStore>,
    hotkey: Mutex<Option<HotkeyManager>>,
    hotkey_error: Mutex<Option<String>>,
    recording: Mutex<Option<(audio::Recording, Instant)>>,
    /// Kept in memory only, so a failed STT call can be retried without re-speaking.
    failed_audio: Mutex<Option<Vec<i16>>>,
    busy: AtomicBool,
    latency: Mutex<Option<Latency>>,
    client: reqwest::Client,
}

#[derive(Serialize, Clone)]
struct OverlayState {
    state: &'static str,
    level: f32,
    message: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn overlay(app: &AppHandle, state: &'static str, message: impl Into<String>) {
    let Some(window) = app.get_webview_window("overlay") else {
        return;
    };
    let _ = app.emit_to(
        "overlay",
        "overlay",
        OverlayState {
            state,
            level: 0.0,
            message: message.into(),
        },
    );
    if state == "hidden" {
        let _ = window.hide();
    } else {
        let _ = window.show();
    }
}

/// Hides the overlay after `millis` unless another dictation started meanwhile.
fn hide_overlay_later(app: &AppHandle, millis: u64) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        let state = app.state::<AppState>();
        if !state.busy.load(Ordering::SeqCst) && state.recording.lock().unwrap().is_none() {
            overlay(&app, "hidden", "");
        }
    });
}

fn notify(app: &AppHandle, body: &str) {
    if !app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .notifications
    {
        return;
    }
    let _ = notify_rust::Notification::new()
        .appname("Voice Prompt")
        .summary("Voice Prompt")
        .body(body)
        .icon("voice-prompt")
        .timeout(5000)
        .show();
}

fn on_hotkey(app: &AppHandle, event: HotkeyEvent) {
    let state = app.state::<AppState>();
    match event {
        HotkeyEvent::Pressed => {
            if state.busy.load(Ordering::SeqCst) {
                return;
            }
            let pressed = Instant::now();
            let settings = state.settings.lock().unwrap().clone();
            let level_app = app.clone();
            match audio::Recording::start(
                &settings.microphone,
                settings.max_record_secs,
                move |level| {
                    let _ = level_app.emit_to(
                        "overlay",
                        "overlay",
                        OverlayState {
                            state: "recording",
                            level,
                            message: String::new(),
                        },
                    );
                },
            ) {
                Ok(recording) => {
                    log::debug!("hotkey→recording {} ms", pressed.elapsed().as_millis());
                    *state.latency.lock().unwrap() = Some(Latency {
                        hotkey_to_recording_ms: pressed.elapsed().as_millis() as u64,
                        ..Default::default()
                    });
                    *state.recording.lock().unwrap() = Some((recording, pressed));
                    if settings.sounds {
                        audio::beep(880.0, 60);
                    }
                    overlay(app, "recording", "");
                }
                Err(error) => {
                    log::warn!("{error:#}");
                    overlay(app, "error", format!("{error}"));
                    notify(app, &format!("{error}"));
                    hide_overlay_later(app, 3000);
                }
            }
        }
        HotkeyEvent::Cancelled => {
            if let Some((recording, _)) = state.recording.lock().unwrap().take() {
                drop(recording.finish());
                overlay(app, "hidden", "");
            }
        }
        HotkeyEvent::Released => {
            let Some((recording, pressed)) = state.recording.lock().unwrap().take() else {
                return;
            };
            let released = Instant::now();
            let samples = recording.finish();
            if state.settings.lock().unwrap().sounds {
                audio::beep(660.0, 60);
            }
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let state = app.state::<AppState>();
                state.busy.store(true, Ordering::SeqCst);
                let result = match samples {
                    Ok(samples) => process(&app, samples, pressed, released).await,
                    Err(error) => Err(error),
                };
                state.busy.store(false, Ordering::SeqCst);
                if let Err(error) = result {
                    log::warn!("dictation failed: {error:#}");
                    overlay(&app, "error", format!("{error}"));
                    notify(&app, &format!("{error}"));
                    hide_overlay_later(&app, 4000);
                }
            });
        }
    }
}

/// The voice pipeline: STT → optional cleanup → insertion, never dropping a successful transcript.
async fn process(
    app: &AppHandle,
    samples: Vec<i16>,
    pressed: Instant,
    released: Instant,
) -> Result<()> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let audio_ms = samples.len() as u64 * 1000 / audio::RATE as u64;
    if (released - pressed).as_millis() < settings.min_hold_ms as u128 || audio::is_silent(&samples)
    {
        overlay(app, "hidden", "");
        return Ok(());
    }
    let api_key = settings::secret::get()?
        .ok_or_else(|| anyhow!("No 9Router API key. Open Voice Prompt settings → Speech."))?;
    overlay(app, "transcribing", "");
    let stt = OpenAiCompatible {
        client: state.client.clone(),
        base_url: settings.endpoint.clone(),
        api_key: api_key.clone(),
        model: settings.stt_model.clone(),
        timeout: Duration::from_secs(settings.stt_timeout_secs),
        reasoning_effort: String::new(),
    };
    let wav = providers::wav(&samples, audio::RATE);
    let transcript = match stt
        .transcribe(
            wav,
            &settings.language,
            settings::vocabulary_hint(&settings.vocabulary),
        )
        .await
    {
        Ok(transcript) => {
            *state.failed_audio.lock().unwrap() = None;
            transcript
        }
        Err(error) => {
            *state.failed_audio.lock().unwrap() = Some(samples);
            return Err(error.context("Transcription failed (use tray → Retry last recording)"));
        }
    };
    drop(samples);
    let stt_done = Instant::now();
    if transcript.text.is_empty() {
        overlay(app, "error", "Nothing recognised");
        hide_overlay_later(app, 1500);
        return Ok(());
    }

    let mut cleanup_error = None;
    let mut cleaned = None;
    if settings.cleanup_enabled {
        overlay(app, "cleaning", "");
        let cleaner = OpenAiCompatible {
            model: settings.cleanup_model.clone(),
            timeout: Duration::from_secs(settings.cleanup_timeout_secs),
            reasoning_effort: settings.reasoning_effort.clone(),
            ..stt
        };
        match cleaner
            .clean(
                &transcript.text,
                &settings.vocabulary,
                &settings.cleanup_instructions,
            )
            .await
        {
            Ok(text) => cleaned = Some(text),
            Err(error) => {
                log::warn!("cleanup failed, using raw transcript: {error:#}");
                cleanup_error = Some(format!("{error}"));
            }
        }
    }
    let cleanup_done = Instant::now();
    let prompt = cleaned.clone().unwrap_or_else(|| transcript.text.clone());

    // Save first: insertion problems must never lose the prompt.
    let mut entry = Entry {
        id: 0,
        created_at: now_ms(),
        raw: transcript.text.clone(),
        cleaned: cleaned.clone(),
        mode: if settings.cleanup_enabled {
            "clean"
        } else {
            "raw"
        }
        .into(),
        stt_model: settings.stt_model.clone(),
        cleanup_model: settings
            .cleanup_enabled
            .then(|| settings.cleanup_model.clone()),
        error: cleanup_error.clone(),
        duration_ms: audio_ms as i64,
        latency_ms: 0,
    };

    let delivered = deliver(app, &settings, &prompt).await;
    let inserted = Instant::now();
    let latency = Latency {
        hotkey_to_recording_ms: state
            .latency
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |l| l.hotkey_to_recording_ms),
        release_to_stt_ms: (stt_done - released).as_millis() as u64,
        stt_to_cleanup_ms: (cleanup_done - stt_done).as_millis() as u64,
        cleanup_to_insert_ms: (inserted - cleanup_done).as_millis() as u64,
        release_to_prompt_ms: (inserted - released).as_millis() as u64,
        audio_ms,
    };
    log::info!(
        "latency: hotkey→rec {}ms, release→stt {}ms, stt→cleanup {}ms, cleanup→insert {}ms, total {}ms (audio {}ms, {} chars)",
        latency.hotkey_to_recording_ms,
        latency.release_to_stt_ms,
        latency.stt_to_cleanup_ms,
        latency.cleanup_to_insert_ms,
        latency.release_to_prompt_ms,
        audio_ms,
        prompt.chars().count()
    );
    entry.latency_ms = latency.release_to_prompt_ms as i64;
    *state.latency.lock().unwrap() = Some(latency);
    if settings.history_enabled {
        let history = state.history.lock().unwrap();
        if let Err(error) = history.add(&entry) {
            log::warn!("history write failed: {error:#}");
        }
        let _ = history.prune(settings.history_retention_days, now_ms());
    }
    let _ = app.emit("history-changed", ());

    match (delivered, cleanup_error) {
        (Ok(how), None) => overlay(app, "done", how),
        (Ok(how), Some(_)) => overlay(
            app,
            "warning",
            format!("Cleanup failed, raw text {}", how.to_lowercase()),
        ),
        (Err(error), _) => {
            overlay(app, "error", format!("{error}"));
            notify(app, &format!("{error}"));
        }
    }
    hide_overlay_later(app, 1600);
    Ok(())
}

/// Inserts into the focused app, falling back to the clipboard. Returns what happened.
async fn deliver(app: &AppHandle, settings: &Settings, text: &str) -> Result<String> {
    let method = insertion::Method::parse(&settings.insertion_method);
    if !settings.auto_insert || method == insertion::Method::Clipboard {
        insertion::copy_to_clipboard(text)?;
        return Ok("Copied — press Ctrl+V".into());
    }
    // Typing while the user still holds a modifier would turn letters into shortcuts.
    let state = app.state::<AppState>();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let held = state
            .hotkey
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.keys_held());
        if !held {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let text_owned = text.to_string();
    let result =
        tauri::async_runtime::spawn_blocking(move || insertion::insert(method, &text_owned))
            .await?;
    match result {
        Ok(()) => {
            if settings.keep_in_clipboard && method == insertion::Method::Type {
                insertion::copy_to_clipboard(text)?;
            }
            Ok("Inserted".into())
        }
        Err(error) => {
            log::warn!("insertion failed, copying instead: {error:#}");
            insertion::copy_to_clipboard(text)?;
            Ok("Copied — press Ctrl+V".into())
        }
    }
}

// ---------- commands ----------

type CommandResult<T> = Result<T, String>;

fn text_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[tauri::command]
fn get_settings(state: tauri::State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(
    app: AppHandle,
    state: tauri::State<AppState>,
    settings: Settings,
) -> CommandResult<()> {
    hotkey::parse(&settings.shortcut).map_err(text_error)?;
    settings::save(&settings::config_dir(), &settings).map_err(text_error)?;
    settings::apply_autostart(settings.autostart).map_err(text_error)?;
    logger::set_debug(settings.debug_logging);
    if let Some(hotkey) = state.hotkey.lock().unwrap().as_ref() {
        hotkey
            .set_shortcut(&settings.shortcut)
            .map_err(text_error)?;
    }
    *state.settings.lock().unwrap() = settings;
    let _ = app.emit("settings-changed", ());
    Ok(())
}

#[tauri::command]
fn has_api_key() -> CommandResult<bool> {
    settings::secret::get()
        .map(|k| k.is_some())
        .map_err(text_error)
}

#[tauri::command]
fn set_api_key(key: String) -> CommandResult<()> {
    settings::secret::set(key.trim()).map_err(text_error)
}

#[tauri::command]
async fn list_microphones() -> CommandResult<Vec<audio::Microphone>> {
    tauri::async_runtime::spawn_blocking(audio::microphones)
        .await
        .map_err(|e| e.to_string())?
        .map_err(text_error)
}

/// Checks endpoint, key and model names, without sending any audio.
#[tauri::command]
async fn test_connection(state: tauri::State<'_, AppState>) -> CommandResult<String> {
    let settings = state.settings.lock().unwrap().clone();
    let key = settings::secret::get()
        .map_err(text_error)?
        .ok_or("No API key saved")?;
    let started = Instant::now();
    let response = state
        .client
        .get(format!(
            "{}/models",
            settings.endpoint.trim_end_matches('/')
        ))
        .bearer_auth(key)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Cannot reach endpoint: {e}"))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err("The API key was rejected".into());
    }
    if !status.is_success() {
        return Err(format!("Endpoint returned {status}"));
    }
    let json: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    let ids: Vec<&str> = json["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str())
        .collect();
    let mut message = format!("Connected in {} ms.", started.elapsed().as_millis());
    for model in [&settings.stt_model, &settings.cleanup_model] {
        if !ids.is_empty() && !ids.contains(&model.as_str()) {
            message += &format!(" Model '{model}' is not listed by the endpoint.");
        }
    }
    Ok(message)
}

#[tauri::command]
fn history_list(state: tauri::State<AppState>) -> CommandResult<Vec<Entry>> {
    state.history.lock().unwrap().list(500).map_err(text_error)
}

#[tauri::command]
fn history_delete(state: tauri::State<AppState>, id: i64) -> CommandResult<()> {
    state.history.lock().unwrap().delete(id).map_err(text_error)
}

#[tauri::command]
fn history_clear(state: tauri::State<AppState>) -> CommandResult<()> {
    state.history.lock().unwrap().clear().map_err(text_error)
}

#[tauri::command]
fn copy_text(text: String) -> CommandResult<()> {
    insertion::copy_to_clipboard(&text).map_err(text_error)
}

#[derive(Serialize)]
struct Diagnostics {
    version: &'static str,
    session: String,
    desktop: String,
    hotkey: String,
    in_input_group: bool,
    virtual_keyboard: String,
    keyring: String,
    log_file: String,
    memory_mb: f64,
    latency: Option<Latency>,
}

#[tauri::command]
async fn diagnostics(state: tauri::State<'_, AppState>) -> CommandResult<Diagnostics> {
    let hotkey = match state.hotkey_error.lock().unwrap().clone() {
        Some(error) => format!("Unavailable: {error}"),
        None => format!("Listening for {}", state.settings.lock().unwrap().shortcut),
    };
    let virtual_keyboard = match insertion::VirtualKeyboard::connect() {
        Ok(_) => "Available (zwp_virtual_keyboard_v1)".into(),
        Err(error) => format!("Unavailable: {error}"),
    };
    let keyring = match settings::secret::get() {
        Ok(Some(_)) => "Key stored in Secret Service".into(),
        Ok(None) => "No key stored".into(),
        Err(error) => format!("Error: {error}"),
    };
    let memory_mb = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .map(str::to_string)
        })
        .and_then(|l| {
            l.split_whitespace()
                .nth(1)
                .and_then(|kb| kb.parse::<f64>().ok())
        })
        .map_or(0.0, |kb| (kb / 1024.0 * 10.0).round() / 10.0);
    let groups = std::process::Command::new("id")
        .arg("-Gn")
        .output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    Ok(Diagnostics {
        version: env!("CARGO_PKG_VERSION"),
        session: std::env::var("XDG_SESSION_TYPE").unwrap_or_default(),
        desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
        hotkey,
        in_input_group: String::from_utf8_lossy(&groups)
            .split_whitespace()
            .any(|g| g == "input"),
        virtual_keyboard,
        keyring,
        log_file: logger::path().display().to_string(),
        memory_mb,
        latency: state.latency.lock().unwrap().clone(),
    })
}

fn retry_last(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(samples) = state.failed_audio.lock().unwrap().take() else {
        notify(app, "Nothing to retry.");
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        state.busy.store(true, Ordering::SeqCst);
        let now = Instant::now();
        let result = process(&app, samples, now - Duration::from_secs(1), now).await;
        state.busy.store(false, Ordering::SeqCst);
        if let Err(error) = result {
            overlay(&app, "error", format!("{error}"));
            notify(&app, &format!("{error}"));
            hide_overlay_later(&app, 4000);
        }
    });
}

fn show_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Voice Prompt")
        .inner_size(860.0, 640.0)
        .min_inner_size(640.0, 480.0)
        .build();
}

fn create_overlay(app: &AppHandle) -> Result<()> {
    let window =
        WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("index.html#overlay".into()))
            .title("Voice Prompt overlay")
            .inner_size(240.0, 52.0)
            .decorations(false)
            .transparent(true)
            .resizable(false)
            .skip_taskbar(true)
            .always_on_top(true)
            .focused(false)
            .focusable(false)
            .shadow(false)
            .visible(false)
            .build()?;
    // A layer-shell surface never takes keyboard focus, so the target app keeps it.
    use gtk::prelude::*;
    use gtk_layer_shell::LayerShell;
    let gtk_window = window.gtk_window()?;
    if gtk_layer_shell::is_supported() {
        gtk_window.unrealize();
        gtk_window.init_layer_shell();
        gtk_window.set_namespace("voice-prompt-overlay");
        gtk_window.set_layer(gtk_layer_shell::Layer::Overlay);
        gtk_window.set_keyboard_mode(gtk_layer_shell::KeyboardMode::None);
        gtk_window.set_anchor(gtk_layer_shell::Edge::Bottom, true);
        gtk_window.set_layer_shell_margin(gtk_layer_shell::Edge::Bottom, 56);
        gtk_window.set_exclusive_zone(-1);
    } else {
        gtk_window.set_accept_focus(false);
    }
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();
    create_overlay(&handle)?;

    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let open = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let retry = MenuItem::with_id(app, "retry", "Retry last recording", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &retry, &quit])?;
    TrayIconBuilder::with_id("tray")
        .icon(app.default_window_icon().cloned().ok_or("missing icon")?)
        .tooltip("Voice Prompt")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "quit" => app.exit(0),
            "settings" => show_settings(app),
            "retry" => retry_last(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_settings(tray.app_handle());
            }
        })
        .build(app)?;

    let state = app.state::<AppState>();
    let shortcut = state.settings.lock().unwrap().shortcut.clone();
    let hotkey_app = handle.clone();
    match HotkeyManager::start(&shortcut, move |event| on_hotkey(&hotkey_app, event)) {
        Ok(manager) => *state.hotkey.lock().unwrap() = Some(manager),
        Err(error) => {
            log::error!("hotkey unavailable: {error:#}");
            notify(&handle, &format!("Push-to-talk unavailable: {error}"));
            *state.hotkey_error.lock().unwrap() = Some(format!("{error}"));
        }
    }

    let background = std::env::args().any(|a| a == "--background");
    if !background || !matches!(settings::secret::get(), Ok(Some(_))) {
        show_settings(&handle);
    }
    Ok(())
}

fn main() {
    let config = settings::config_dir();
    let data = settings::data_dir();
    let _ = settings::private_dir(&config);
    let _ = settings::private_dir(&data);
    let settings = settings::load(&config);
    logger::init(&data.join("voice-prompt.log"), settings.debug_logging);
    let history =
        HistoryStore::open(&data.join("history.db")).expect("cannot open history database");
    let _ = history.prune(settings.history_retention_days, now_ms());
    if let Err(error) = settings::apply_autostart(settings.autostart) {
        log::warn!("autostart: {error:#}");
    }
    let state = AppState {
        settings: Mutex::new(settings),
        history: Mutex::new(history),
        hotkey: Mutex::new(None),
        hotkey_error: Mutex::new(None),
        recording: Mutex::new(None),
        failed_audio: Mutex::new(None),
        busy: AtomicBool::new(false),
        latency: Mutex::new(None),
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("http client"),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_settings(app)
        }))
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            has_api_key,
            set_api_key,
            list_microphones,
            test_connection,
            history_list,
            history_delete,
            history_clear,
            copy_text,
            diagnostics
        ])
        .setup(setup)
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_, event| {
            // Closing settings keeps the app running in the tray.
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
