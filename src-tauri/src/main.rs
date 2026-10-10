#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use anyhow::{anyhow, Result};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use voice_prompt::history::{now_ms, Entry, HistoryStore};
use voice_prompt::hotkey::{self, HotkeyEvent, HotkeyManager};
use voice_prompt::pipeline::{Latency, Outcome, Pipeline, Recording, Snapshot};
use voice_prompt::settings::{self, Settings};
use voice_prompt::{audio, delivery, insertion, logger, providers, streaming, target};

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// Exclusive ownership of the pipeline; released on drop, including early returns.
struct Busy(Arc<AtomicBool>);

impl Busy {
    fn acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(flag.clone()))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct Session {
    /// Owned from key press to the end of processing, so a retry cannot run alongside.
    busy: Busy,
    recording: audio::Recording,
    pressed: Instant,
    capture_started: Instant,
    latency: Latency,
    snapshot: Snapshot,
    /// Live transcription; dropping it cancels the socket without finalizing.
    stream: Option<streaming::StreamSession>,
}

struct AppState {
    settings: Mutex<Settings>,
    pipeline: Pipeline,
    hotkey: Arc<Mutex<Option<HotkeyManager>>>,
    hotkey_error: Mutex<Option<String>>,
    recording: Mutex<Option<Session>>,
    busy: Arc<AtomicBool>,
    client: reqwest::Client,
    /// COSMIC active-window tracking; `None` on compositors without the protocols.
    windows: Option<target::Tracker>,
}

#[derive(Serialize, Clone)]
struct OverlayState {
    state: &'static str,
    level: f32,
    message: String,
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
            let Some(busy) = Busy::acquire(&state.busy) else {
                return;
            };
            let pressed = Instant::now();
            // Before the overlay or any cue can disturb focus.
            let target = state.windows.as_ref().and_then(target::Tracker::active);
            let snapshot = Snapshot::take(state.settings.lock().unwrap().clone(), target);
            let settings = &snapshot.settings;
            // The queue exists before token and connection work starts, so early speech is kept.
            let (stream, sink) = if settings.streaming {
                let (stream, sink) =
                    streaming::start(state.client.clone(), settings, snapshot.keyterms());
                (Some(stream), Some(sink))
            } else {
                (None, None)
            };
            let capture_started = Instant::now();
            let level_app = app.clone();
            let sounds = settings.sounds;
            match audio::Recording::start(
                &settings.microphone,
                settings.max_record_secs,
                move |chunk| {
                    if let Some(sink) = &sink {
                        sink.push(chunk);
                    }
                    let level = audio::rms(chunk);
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
                    let mut latency = Latency::default();
                    latency.hotkey_to_capture_ms = ms(pressed);
                    *state.recording.lock().unwrap() = Some(Session {
                        busy,
                        recording,
                        pressed,
                        capture_started,
                        latency,
                        snapshot,
                        stream,
                    });
                    if sounds {
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
            if let Some(session) = state.recording.lock().unwrap().take() {
                drop(session.recording.finish());
                log::info!(
                    "dictation outcome=cancelled held={}",
                    session.pressed.elapsed().as_millis()
                );
                overlay(app, "hidden", "");
            }
        }
        HotkeyEvent::Released => {
            let Some(session) = state.recording.lock().unwrap().take() else {
                return;
            };
            let released = Instant::now();
            let mut latency = session.latency;
            let (samples, first_audio) = session.recording.finish();
            latency.finish_ms = ms(released);
            latency.capture_to_first_audio_ms = first_audio.map(|first| {
                first
                    .saturating_duration_since(session.capture_started)
                    .as_millis() as u64
            });
            if session.snapshot.settings.sounds {
                audio::beep(660.0, 60);
            }
            let recording = samples.map(|samples| Recording {
                samples,
                pressed: Some(session.pressed),
                released,
                held: released - session.pressed,
                stream: session.stream,
                latency,
            });
            let busy = session.busy;
            let snapshot = session.snapshot;
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                match recording {
                    Ok(recording) => process(&app, &snapshot, recording).await,
                    Err(error) => report(&app, &error),
                }
                drop(busy);
            });
        }
    }
}

fn report(app: &AppHandle, error: &anyhow::Error) {
    log::warn!("dictation failed: {error:#}");
    overlay(app, "error", format!("{error}"));
    notify(app, &format!("{error}"));
    hide_overlay_later(app, 4000);
}

/// Runs the pipeline and shows how it went.
async fn process(app: &AppHandle, snapshot: &Snapshot, recording: Recording) {
    let state = app.state::<AppState>();
    let outcome = state
        .pipeline
        .run(snapshot, recording, |stage| overlay(app, stage, ""))
        .await;
    match outcome {
        Err(error) => return report(app, &error),
        Ok(Outcome::Ignored) => return overlay(app, "hidden", ""),
        Ok(Outcome::Empty) => {
            overlay(app, "error", "Nothing recognised");
            return hide_overlay_later(app, 1500);
        }
        Ok(Outcome::Delivered {
            message,
            cleanup_failed: false,
        }) => overlay(app, "done", message),
        Ok(Outcome::Delivered {
            message,
            cleanup_failed: true,
        }) => overlay(
            app,
            "warning",
            format!("Cleanup failed, raw text {}", message.to_lowercase()),
        ),
        Ok(Outcome::Undelivered(error)) => {
            let message = format!("{error} (tray → Copy last prompt)");
            overlay(app, "error", &message);
            notify(app, &message);
        }
    }
    let _ = app.emit("history-changed", ());
    hide_overlay_later(app, 1600);
}

/// The app's providers, built per dictation from the stored key.
fn connect(client: reqwest::Client) -> voice_prompt::pipeline::Connect {
    Box::new(move |settings| {
        let key = settings::secret::get()?
            .ok_or_else(|| anyhow!("No 9Router API key. Open Voice Prompt settings → Speech."))?;
        Ok(providers::connect(client.clone(), settings, key))
    })
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
fn has_muse_key() -> CommandResult<bool> {
    settings::secret::get_for(settings::secret::MUSE)
        .map(|k| k.is_some())
        .map_err(text_error)
}

#[tauri::command]
fn set_muse_key(key: String) -> CommandResult<()> {
    settings::secret::set_for(settings::secret::MUSE, key.trim()).map_err(text_error)
}

/// Handshakes with Muse using the stored key, without sending audio.
#[tauri::command]
async fn test_muse() -> CommandResult<String> {
    let started = Instant::now();
    streaming::muse_check().await.map_err(|e| e.to_string())?;
    Ok(format!(
        "Muse accepted the key ({} ms).",
        started.elapsed().as_millis()
    ))
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
    state
        .pipeline
        .history
        .lock()
        .unwrap()
        .list(500)
        .map_err(text_error)
}

#[tauri::command]
fn history_delete(state: tauri::State<AppState>, id: i64) -> CommandResult<()> {
    state
        .pipeline
        .history
        .lock()
        .unwrap()
        .delete(id)
        .map_err(text_error)
}

#[tauri::command]
fn history_clear(state: tauri::State<AppState>) -> CommandResult<()> {
    state
        .pipeline
        .history
        .lock()
        .unwrap()
        .clear()
        .map_err(text_error)
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
        latency: state.pipeline.last_latency.lock().unwrap().clone(),
    })
}

fn retry_last(app: &AppHandle) {
    let state = app.state::<AppState>();
    let Some(busy) = Busy::acquire(&state.busy) else {
        notify(app, "A dictation is still being processed.");
        return;
    };
    let Some(samples) = state.pipeline.take_failed_audio() else {
        notify(app, "Nothing to retry.");
        return;
    };
    // A retry has no press-time window, so delivery falls back to the clipboard.
    let snapshot = Snapshot::take(state.settings.lock().unwrap().clone(), None);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        process(&app, &snapshot, Recording::retry(samples)).await;
        drop(busy);
    });
}

fn copy_last_prompt(app: &AppHandle) {
    let prompt = app
        .state::<AppState>()
        .pipeline
        .last_prompt
        .lock()
        .unwrap()
        .clone();
    match prompt.map(|text| insertion::copy_to_clipboard(&text)) {
        None => notify(app, "No prompt yet."),
        Some(Ok(())) => notify(app, "Last prompt copied — press Ctrl+V."),
        Some(Err(error)) => notify(app, &format!("{error}")),
    }
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
    let copy_last = MenuItem::with_id(app, "copy-last", "Copy last prompt", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &retry, &copy_last, &quit])?;
    TrayIconBuilder::with_id("tray")
        .icon(app.default_window_icon().cloned().ok_or("missing icon")?)
        .tooltip("Voice Prompt")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "quit" => app.exit(0),
            "settings" => show_settings(app),
            "retry" => retry_last(app),
            "copy-last" => copy_last_prompt(app),
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
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("http client");
    let windows = target::Tracker::start()
        .map_err(|error| log::info!("window tracking unavailable: {error:#}"))
        .ok();
    let hotkey: Arc<Mutex<Option<HotkeyManager>>> = Default::default();
    let held = hotkey.clone();
    let desktop = delivery::System {
        windows: windows.clone(),
        keys_held: Box::new(move || {
            held.lock()
                .unwrap()
                .as_ref()
                .is_some_and(HotkeyManager::keys_held)
        }),
    };
    let state = AppState {
        settings: Mutex::new(settings),
        pipeline: Pipeline::new(history, connect(client.clone()), Arc::new(desktop)),
        hotkey,
        hotkey_error: Mutex::new(None),
        recording: Mutex::new(None),
        busy: Arc::new(AtomicBool::new(false)),
        client,
        windows,
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
            has_muse_key,
            set_muse_key,
            test_muse,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_is_exclusive_and_released_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        let first = Busy::acquire(&flag).expect("free pipeline");
        assert!(
            Busy::acquire(&flag).is_none(),
            "second session must not start"
        );
        drop(first);
        assert!(Busy::acquire(&flag).is_some());
    }

    #[test]
    fn busy_race_has_one_winner() {
        let flag = Arc::new(AtomicBool::new(false));
        let winners: usize = (0..8)
            .map(|_| {
                let flag = flag.clone();
                std::thread::spawn(move || Busy::acquire(&flag).map(std::mem::forget).is_some())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap() as usize)
            .sum();
        assert_eq!(winners, 1);
    }
}
