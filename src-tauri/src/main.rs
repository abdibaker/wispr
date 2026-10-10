#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod audio;
mod history;
mod hotkey;
mod insertion;
mod logger;
mod providers;
mod settings;
mod target;

use anyhow::{anyhow, Result};
use history::{Entry, HistoryStore};
use hotkey::{HotkeyEvent, HotkeyManager};
use providers::{OpenAiCompatible, PromptCleaner, SpeechProvider};
use serde::Serialize;
use settings::Settings;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

/// Per-dictation stage timings, logged as one `latency` line (`bench/run.py latency` summarizes them).
#[derive(Serialize, Clone, Default, Debug)]
struct Latency {
    hotkey_to_capture_ms: u64,
    /// `None` when no audio chunk arrived; never logged as zero.
    capture_to_first_audio_ms: Option<u64>,
    finish_ms: u64,
    keyring_ms: u64,
    wav_ms: u64,
    stt_setup_ms: u64,
    stt_wait_ms: u64,
    stt_read_ms: u64,
    stt_normalize_ms: u64,
    cleanup_setup_ms: u64,
    cleanup_wait_ms: u64,
    cleanup_read_ms: u64,
    cleanup_normalize_ms: u64,
    /// Wall time of the cleanup step, including retries and a failed attempt.
    cleanup_total_ms: u64,
    cleanup_attempts: u32,
    history_ms: u64,
    held_wait_ms: u64,
    insert_setup_ms: u64,
    insert_keys_ms: u64,
    clipboard_ms: u64,
    /// Release → text delivered; the sum of the stages above plus untimed glue.
    release_to_delivered_ms: u64,
    audio_ms: u64,
    /// Time since the previous provider request; above the 90 s pool timeout the
    /// connection is cold.
    idle_ms: u64,
    /// cleaned | raw (cleanup off) | fallback_<failure> | stt_<failure> | empty
    outcome: String,
    /// typed | pasted | pasted_terminal | copied_<reason> | failed
    delivery: String,
    /// confirmed | changed | unknown: the press-time window versus the window at delivery.
    destination: &'static str,
}

impl Latency {
    fn log(&self, chars: usize) {
        let first_audio = self
            .capture_to_first_audio_ms
            .map_or("na".into(), |ms| ms.to_string());
        let timed = self.finish_ms
            + self.keyring_ms
            + self.wav_ms
            + self.stt_setup_ms
            + self.stt_wait_ms
            + self.stt_read_ms
            + self.stt_normalize_ms
            + self.cleanup_total_ms
            + self.history_ms
            + self.held_wait_ms
            + self.insert_setup_ms
            + self.insert_keys_ms
            + self.clipboard_ms;
        log::info!(
            "latency total={} audio={} chars={} idle={} capture={} first_audio={} finish={} keyring={} wav={} \
             stt_setup={} stt_wait={} stt_read={} stt_norm={} cleanup_setup={} cleanup_wait={} cleanup_read={} \
             cleanup_norm={} cleanup_total={} cleanup_attempts={} history={} held_wait={} insert_setup={} \
             insert_keys={} clipboard={} untimed={} outcome={} delivery={} destination={}",
            self.release_to_delivered_ms,
            self.audio_ms,
            chars,
            self.idle_ms,
            self.hotkey_to_capture_ms,
            first_audio,
            self.finish_ms,
            self.keyring_ms,
            self.wav_ms,
            self.stt_setup_ms,
            self.stt_wait_ms,
            self.stt_read_ms,
            self.stt_normalize_ms,
            self.cleanup_setup_ms,
            self.cleanup_wait_ms,
            self.cleanup_read_ms,
            self.cleanup_normalize_ms,
            self.cleanup_total_ms,
            self.cleanup_attempts,
            self.history_ms,
            self.held_wait_ms,
            self.insert_setup_ms,
            self.insert_keys_ms,
            self.clipboard_ms,
            self.release_to_delivered_ms.saturating_sub(timed),
            or_na(&self.outcome),
            or_na(&self.delivery),
            or_na(self.destination),
        );
    }
}

fn or_na(label: &str) -> &str {
    if label.is_empty() {
        "na"
    } else {
        label
    }
}

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
    /// The window active at key press: the intended destination.
    target: Option<target::Window>,
}

struct AppState {
    settings: Mutex<Settings>,
    history: Mutex<HistoryStore>,
    hotkey: Mutex<Option<HotkeyManager>>,
    hotkey_error: Mutex<Option<String>>,
    recording: Mutex<Option<Session>>,
    /// Kept in memory only, so a failed STT call can be retried without re-speaking.
    failed_audio: Mutex<Option<Vec<i16>>>,
    /// The last finished prompt, so text whose insertion failed can still be recovered
    /// (tray → Copy last prompt) even with history disabled.
    last_prompt: Mutex<Option<String>>,
    busy: Arc<AtomicBool>,
    latency: Mutex<Option<Latency>>,
    started: Instant,
    last_request: Mutex<Option<Instant>>,
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
            let Some(busy) = Busy::acquire(&state.busy) else {
                return;
            };
            let pressed = Instant::now();
            // Before the overlay or any cue can disturb focus.
            let target = state.windows.as_ref().and_then(target::Tracker::active);
            let settings = state.settings.lock().unwrap().clone();
            let capture_started = Instant::now();
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
                    let latency = Latency {
                        hotkey_to_capture_ms: ms(pressed),
                        ..Default::default()
                    };
                    *state.recording.lock().unwrap() = Some(Session {
                        busy,
                        recording,
                        pressed,
                        capture_started,
                        latency,
                        target,
                    });
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
            if state.settings.lock().unwrap().sounds {
                audio::beep(660.0, 60);
            }
            let busy = session.busy;
            let target = session.target;
            let held = released - session.pressed;
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let result = match samples {
                    Ok(samples) => process(&app, samples, latency, released, held, target).await,
                    Err(error) => Err(error),
                };
                drop(busy);
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
    mut latency: Latency,
    released: Instant,
    held: Duration,
    target: Option<target::Window>,
) -> Result<()> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let audio_ms = samples.len() as u64 * 1000 / audio::RATE as u64;
    latency.audio_ms = audio_ms;
    if held.as_millis() < settings.min_hold_ms as u128 || audio::is_silent(&samples) {
        overlay(app, "hidden", "");
        return Ok(());
    }
    let started = Instant::now();
    let api_key = settings::secret::get()?
        .ok_or_else(|| anyhow!("No 9Router API key. Open Voice Prompt settings → Speech."))?;
    latency.keyring_ms = ms(started);
    overlay(app, "transcribing", "");
    latency.idle_ms = ms(state.last_request.lock().unwrap().unwrap_or(state.started));
    let stt = OpenAiCompatible {
        client: state.client.clone(),
        base_url: settings.endpoint.clone(),
        api_key: api_key.clone(),
        model: settings.stt_model.clone(),
        timeout: Duration::from_secs(settings.stt_timeout_secs),
        reasoning_effort: String::new(),
    };
    let encoding = Instant::now();
    let wav = providers::wav(&samples, audio::RATE);
    latency.wav_ms = ms(encoding);
    // A global hint steers unrelated prose toward product names, so send vocabulary only
    // where technical dictation is likely: editors, terminals, and windows showing a file.
    let mut vocabulary = target
        .as_ref()
        .map_or(vec![], |w| settings::title_terms(&w.title));
    let technical =
        !vocabulary.is_empty() || target.as_ref().is_some_and(target::Window::is_technical);
    vocabulary.extend(settings.vocabulary.iter().cloned());
    let hint = technical
        .then(|| settings::vocabulary_hint(&vocabulary))
        .flatten();
    let transcript = match stt.transcribe(wav, &settings.language, hint).await {
        Ok(transcript) => {
            *state.failed_audio.lock().unwrap() = None;
            latency.stt_setup_ms = transcript.timing.setup_ms;
            latency.stt_wait_ms = transcript.timing.wait_ms;
            latency.stt_read_ms = transcript.timing.body_ms;
            latency.stt_normalize_ms = transcript.timing.normalize_ms;
            transcript
        }
        Err(error) => {
            *state.failed_audio.lock().unwrap() = Some(samples);
            latency.outcome = format!(
                "stt_{}",
                providers::Failure::of(&error).map_or("error", providers::Failure::name)
            );
            latency.delivery = "none".into();
            latency.release_to_delivered_ms = ms(released);
            latency.log(0);
            return Err(error.context("Transcription failed (use tray → Retry last recording)"));
        }
    };
    drop(samples);
    *state.last_request.lock().unwrap() = Some(Instant::now());
    if transcript.text.is_empty() {
        latency.outcome = "empty".into();
        latency.delivery = "none".into();
        latency.release_to_delivered_ms = ms(released);
        latency.log(0);
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
        // Only terms the transcript appears to mention, so prose is not rewritten toward them.
        let terms: Vec<String> = settings::relevant_terms(&vocabulary, &transcript.text)
            .into_iter()
            .map(String::from)
            .collect();
        let cleaning = Instant::now();
        let result = cleaner
            .clean(&transcript.text, &terms, &settings.cleanup_instructions)
            .await;
        latency.cleanup_total_ms = ms(cleaning);
        match result {
            Ok(result) => {
                latency.cleanup_setup_ms = result.timing.setup_ms;
                latency.cleanup_wait_ms = result.timing.wait_ms;
                latency.cleanup_read_ms = result.timing.body_ms;
                latency.cleanup_normalize_ms = result.timing.normalize_ms;
                latency.cleanup_attempts = result.attempts;
                latency.outcome = "cleaned".into();
                cleaned = Some(result.text);
            }
            Err(error) => {
                log::warn!("cleanup failed, using raw transcript: {error:#}");
                latency.outcome = format!(
                    "fallback_{}",
                    providers::Failure::of(&error).map_or("error", providers::Failure::name)
                );
                cleanup_error = Some(format!("{error}"));
            }
        }
    } else {
        latency.outcome = "raw".into();
    }
    *state.last_request.lock().unwrap() = Some(Instant::now());
    let prompt = cleaned.clone().unwrap_or_else(|| transcript.text.clone());
    *state.last_prompt.lock().unwrap() = Some(prompt.clone());

    // Save first: insertion problems (or a hung compositor) must never lose the prompt.
    let saving = Instant::now();
    let entry = Entry {
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
        latency_ms: released.elapsed().as_millis() as i64,
    };
    let saved = if settings.history_enabled {
        state
            .history
            .lock()
            .unwrap()
            .add(&entry)
            .map_err(|error| log::warn!("history write failed: {error:#}"))
            .ok()
    } else {
        None
    };
    latency.history_ms = ms(saving);

    let delivered = deliver(app, &settings, &prompt, target.as_ref(), &mut latency).await;
    if delivered.is_err() {
        latency.delivery = "failed".into();
    }
    latency.release_to_delivered_ms = ms(released);
    latency.log(prompt.chars().count());
    if let Some(id) = saved {
        let history = state.history.lock().unwrap();
        let _ = history.set_latency(id, latency.release_to_delivered_ms as i64);
        let _ = history.prune(settings.history_retention_days, now_ms());
    }
    *state.latency.lock().unwrap() = Some(latency);
    let _ = app.emit("history-changed", ());

    match (delivered, cleanup_error) {
        (Ok(how), None) => overlay(app, "done", how),
        (Ok(how), Some(_)) => overlay(
            app,
            "warning",
            format!("Cleanup failed, raw text {}", how.to_lowercase()),
        ),
        (Err(error), _) => {
            let message = format!("{error} (tray → Copy last prompt)");
            overlay(app, "error", &message);
            notify(app, &message);
        }
    }
    hide_overlay_later(app, 1600);
    Ok(())
}

/// Inserts into the focused app, falling back to the clipboard. Returns what happened.
/// Text is inserted only into the window that was active at key press; we never move focus
/// back, so a changed window gets the clipboard instead.
async fn deliver(
    app: &AppHandle,
    settings: &Settings,
    text: &str,
    target: Option<&target::Window>,
    latency: &mut Latency,
) -> Result<String> {
    let copy = |latency: &mut Latency, reason: &str| -> Result<()> {
        let started = Instant::now();
        let result = insertion::copy_to_clipboard(text);
        latency.clipboard_ms += ms(started);
        latency.delivery = format!("copied_{reason}");
        result
    };
    let configured = insertion::Method::parse(&settings.insertion_method);
    if !settings.auto_insert || configured == insertion::Method::Clipboard {
        copy(latency, "setting")?;
        return Ok("Copied — press Ctrl+V".into());
    }
    // Typing while the user still holds a modifier would turn letters into shortcuts.
    let waiting = Instant::now();
    let held = wait_for_keys_released(app, Duration::from_secs(3)).await;
    latency.held_wait_ms = ms(waiting);
    if held {
        log::warn!("keys still held after 3 s; copying instead of typing");
        copy(latency, "held_keys")?;
        return Ok("Keys still held — copied, press Ctrl+V".into());
    }
    // Revalidate last, after every wait, right before keys are sent.
    let state = app.state::<AppState>();
    let now = state.windows.as_ref().map(target::Tracker::active);
    let destination = match &now {
        Some(now) => target::revalidate(target, now.as_ref()),
        None => target::Destination::Unknown,
    };
    latency.destination = destination.label();
    log::info!(
        "destination {} app={}",
        destination.label(),
        target.map_or("none", |w| w.app_id.as_str())
    );
    let window = match destination {
        target::Destination::Confirmed(window) => Some(window),
        target::Destination::Changed { from, to } => {
            log::info!(
                "target changed from {} to {}; copying instead of inserting",
                from.app_id,
                to.map_or("none".into(), |w| w.app_id)
            );
            copy(latency, "target_changed")?;
            return Ok("Window changed — copied, press Ctrl+V".into());
        }
        // Tracking works but no window was active at press: nothing to insert into.
        target::Destination::Unknown if now.is_some() => {
            copy(latency, "no_target")?;
            return Ok("No target window — copied, press Ctrl+V".into());
        }
        // No tracking on this compositor: keep the configured behavior.
        target::Destination::Unknown => None,
    };
    let method = insertion::choose(configured, window.as_ref(), text);
    let text_owned = text.to_string();
    let result =
        tauri::async_runtime::spawn_blocking(move || insertion::insert(method, &text_owned))
            .await?;
    match result {
        Ok(timing) => {
            latency.insert_setup_ms = timing.setup_ms;
            latency.insert_keys_ms = timing.keys_ms;
            latency.clipboard_ms = timing.clipboard_ms;
            latency.delivery = method.name().into();
            if settings.keep_in_clipboard && method == insertion::Method::Type {
                // The text is already inserted; a clipboard failure here is not a delivery failure.
                let started = Instant::now();
                if let Err(error) = insertion::copy_to_clipboard(text) {
                    log::warn!("keep-in-clipboard failed: {error:#}");
                }
                latency.clipboard_ms += ms(started);
            }
            Ok("Inserted".into())
        }
        Err(error) => {
            log::warn!("insertion failed, copying instead: {error:#}");
            copy(latency, "insert_failed")?;
            Ok("Copied — press Ctrl+V".into())
        }
    }
}

/// Polls physical key state for up to `limit`; returns true if keys are still held.
async fn wait_for_keys_released(app: &AppHandle, limit: Duration) -> bool {
    let state = app.state::<AppState>();
    let deadline = Instant::now() + limit;
    loop {
        let held = state
            .hotkey
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.keys_held());
        if !held || Instant::now() >= deadline {
            return held;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
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
    let Some(busy) = Busy::acquire(&state.busy) else {
        notify(app, "A dictation is still being processed.");
        return;
    };
    let Some(samples) = state.failed_audio.lock().unwrap().take() else {
        notify(app, "Nothing to retry.");
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        // A retry has no press-time window, so delivery falls back to the clipboard.
        let result = process(
            &app,
            samples,
            Latency::default(),
            Instant::now(),
            Duration::MAX,
            None,
        )
        .await;
        drop(busy);
        if let Err(error) = result {
            overlay(&app, "error", format!("{error}"));
            notify(&app, &format!("{error}"));
            hide_overlay_later(&app, 4000);
        }
    });
}

fn copy_last_prompt(app: &AppHandle) {
    let prompt = app.state::<AppState>().last_prompt.lock().unwrap().clone();
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
    let state = AppState {
        settings: Mutex::new(settings),
        history: Mutex::new(history),
        hotkey: Mutex::new(None),
        hotkey_error: Mutex::new(None),
        recording: Mutex::new(None),
        failed_audio: Mutex::new(None),
        last_prompt: Mutex::new(None),
        busy: Arc::new(AtomicBool::new(false)),
        latency: Mutex::new(None),
        started: Instant::now(),
        last_request: Mutex::new(None),
        client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .expect("http client"),
        windows: target::Tracker::start()
            .map_err(|error| log::info!("window tracking unavailable: {error:#}"))
            .ok(),
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
