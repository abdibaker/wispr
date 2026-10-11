//! The voice pipeline: one recording → STT (streamed, else batch) → optional cleanup → history
//! → delivery. A successful transcript is never dropped: it is kept as the last prompt and
//! saved before delivery, and audio whose STT failed is kept for a retry.
use crate::delivery::{self, Desktop};
use crate::history::{now_ms, Entry, HistoryStore};
use crate::providers::{self, Failure, Providers, Transcript};
use crate::settings::{self, Settings};
use crate::streaming::{self, StreamSession};
use crate::target::Window;
use crate::{audio, target};
use anyhow::Result;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// Everything a dictation uses that is decided at key press. Settings changed while the key
/// is held apply to the next dictation.
pub struct Snapshot {
    pub settings: Settings,
    /// The window active at key press: the intended destination.
    pub target: Option<Window>,
    /// Window file names plus the global vocabulary; see `take`.
    pub vocabulary: Vec<String>,
    /// Whether STT is biased toward `vocabulary`.
    pub technical: bool,
}

impl Snapshot {
    /// A global STT hint steers unrelated prose toward product names, so STT gets terms only
    /// where technical dictation is likely: editors, terminals, and windows showing a file.
    /// Cleanup always filters the list to terms the transcript mentions.
    pub fn take(settings: Settings, target: Option<Window>) -> Self {
        let mut vocabulary = target
            .as_ref()
            .map_or(vec![], |w| settings::title_terms(&w.title));
        let technical =
            !vocabulary.is_empty() || target.as_ref().is_some_and(target::Window::is_technical);
        vocabulary.extend(settings.vocabulary.iter().cloned());
        Self {
            settings,
            target,
            vocabulary,
            technical,
        }
    }

    /// Terms for live STT.
    pub fn keyterms(&self) -> &[String] {
        if self.technical {
            &self.vocabulary
        } else {
            &[]
        }
    }
}

/// One finished recording on its way to delivery.
pub struct Recording {
    pub samples: Vec<i16>,
    /// `None` for a retry, which has no live session.
    pub pressed: Option<Instant>,
    pub released: Instant,
    pub held: Duration,
    /// Live transcription; dropping it cancels the socket without finalizing.
    pub stream: Option<StreamSession>,
    /// Capture timings measured so far.
    pub latency: Latency,
}

impl Recording {
    /// Audio kept from a failed STT call, transcribed again with batch STT.
    pub fn retry(samples: Vec<i16>) -> Self {
        Self {
            samples,
            pressed: None,
            released: Instant::now(),
            held: Duration::MAX,
            stream: None,
            latency: Latency::default(),
        }
    }
}

/// How a dictation ended, short of an STT failure (returned as an error).
#[derive(Debug)]
pub enum Outcome {
    /// Too short or silent: nothing was sent anywhere.
    Ignored,
    /// STT returned no text.
    Empty,
    /// `message` says how, e.g. "Inserted"; `cleanup_failed` means raw text was delivered.
    Delivered {
        message: &'static str,
        cleanup_failed: bool,
    },
    /// Even the clipboard failed; the prompt is in history and the last prompt.
    Undelivered(anyhow::Error),
}

/// Builds the providers for one dictation, reading the API key.
pub type Connect = Box<dyn Fn(&Settings) -> Result<Providers> + Send + Sync>;

pub struct Pipeline {
    pub history: Mutex<HistoryStore>,
    /// The last finished prompt, so text whose insertion failed can still be recovered
    /// (tray → Copy last prompt) even with history disabled.
    pub last_prompt: Mutex<Option<String>>,
    pub last_latency: Mutex<Option<Latency>>,
    /// Kept in memory only, so a failed STT call can be retried without re-speaking.
    failed_audio: Mutex<Option<Vec<i16>>>,
    started: Instant,
    last_request: Mutex<Option<Instant>>,
    connect: Connect,
    desktop: Arc<dyn Desktop>,
}

impl Pipeline {
    pub fn new(history: HistoryStore, connect: Connect, desktop: Arc<dyn Desktop>) -> Self {
        Self {
            history: Mutex::new(history),
            last_prompt: Mutex::new(None),
            last_latency: Mutex::new(None),
            failed_audio: Mutex::new(None),
            started: Instant::now(),
            last_request: Mutex::new(None),
            connect,
            desktop,
        }
    }

    /// Audio whose STT failed, now owned by the caller.
    pub fn take_failed_audio(&self) -> Option<Vec<i16>> {
        self.failed_audio.lock().unwrap().take()
    }

    /// Runs one dictation. `stage` hears "transcribing" and "cleaning" as they start.
    /// Fails when there is no API key or STT fails (the audio is kept for a retry).
    pub async fn run(
        &self,
        snapshot: &Snapshot,
        recording: Recording,
        stage: impl Fn(&'static str),
    ) -> Result<Outcome> {
        let Recording {
            samples,
            pressed,
            released,
            held,
            stream,
            mut latency,
        } = recording;
        let settings = &snapshot.settings;
        let audio_ms = samples.len() as u64 * 1000 / audio::RATE as u64;
        latency.audio_ms = audio_ms;
        if held.as_millis() < settings.min_hold_ms as u128 || audio::is_silent(&samples) {
            return Ok(Outcome::Ignored);
        }
        let started = Instant::now();
        let Providers { stt, cleaner } = (self.connect)(settings)?;
        latency.keyring_ms = ms(started);
        stage("transcribing");
        latency.idle_ms = ms(self.last_request.lock().unwrap().unwrap_or(self.started));

        latency.stt_mode = "batch";
        let mut streamed = None;
        if let Some(stream) = stream {
            let finishing = Instant::now();
            let conclusion = stream.conclude().await;
            latency.stt_wait_ms = ms(finishing);
            if let Some(pressed) = pressed {
                latency.record_stream(&conclusion.stats, pressed, released);
            }
            match conclusion.transcript {
                Ok(transcript) => {
                    latency.stt_mode = conclusion.mode;
                    streamed = Some(transcript);
                }
                Err(reason) => {
                    latency.stt_mode = "batch_fallback";
                    latency.fallback_reason = reason;
                    log::warn!(
                        "streaming STT failed ({reason}); replaying the full recording to batch STT"
                    );
                }
            }
        }
        let (result, stt_model) = match streamed {
            Some(streamed) => (
                Ok(Transcript {
                    text: streamed.text,
                    timing: Default::default(),
                }),
                streamed.model,
            ),
            None => {
                let encoding = Instant::now();
                let wav = providers::wav(&samples, audio::RATE);
                latency.wav_ms = ms(encoding);
                let hint = snapshot
                    .technical
                    .then(|| settings::vocabulary_hint(&snapshot.vocabulary))
                    .flatten();
                (
                    stt.transcribe(wav, &settings.language, hint).await,
                    settings.stt_model.clone(),
                )
            }
        };
        latency.release_to_stt_ms = Some(ms(released));
        let transcript = match result {
            Ok(transcript) => {
                *self.failed_audio.lock().unwrap() = None;
                latency.stt_setup_ms = transcript.timing.setup_ms;
                latency.stt_wait_ms += transcript.timing.wait_ms;
                latency.stt_read_ms = transcript.timing.body_ms;
                latency.stt_normalize_ms = transcript.timing.normalize_ms;
                transcript
            }
            Err(error) => {
                *self.failed_audio.lock().unwrap() = Some(samples);
                latency.outcome = format!("stt_{}", failure(&error));
                latency.delivery = "none".into();
                latency.release_to_delivered_ms = ms(released);
                latency.log(0);
                return Err(error.context("Transcription failed (use tray → Retry last recording)"));
            }
        };
        drop(samples);
        *self.last_request.lock().unwrap() = Some(Instant::now());
        if transcript.text.is_empty() {
            latency.outcome = "empty".into();
            latency.delivery = "none".into();
            latency.release_to_delivered_ms = ms(released);
            latency.log(0);
            return Ok(Outcome::Empty);
        }

        let mut cleanup_error = None;
        let mut cleaned = None;
        let mut cleanup_model = None;
        if settings.cleanup_enabled {
            stage("cleaning");
            // Only terms the transcript appears to mention, so prose is not rewritten toward them.
            let terms: Vec<String> =
                settings::relevant_terms(&snapshot.vocabulary, &transcript.text)
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
                    latency.cleanup_provider = result.provider;
                    latency.cleanup_model = result.model.clone();
                    latency.cleanup_fallback_ms = result.failed_ms;
                    latency.outcome = match result.fallback_reason {
                        Some(reason) => {
                            latency.cleanup_fallback_reason = reason;
                            "cleaned_fallback".into()
                        }
                        None => "cleaned".into(),
                    };
                    cleanup_model = Some(result.model);
                    cleaned = Some(result.text);
                }
                Err(error) => {
                    log::warn!("cleanup failed, using raw transcript: {error:#}");
                    latency.outcome = format!("fallback_{}", failure(&error));
                    cleanup_error = Some(format!("{error}"));
                }
            }
        } else {
            latency.outcome = "raw".into();
        }
        *self.last_request.lock().unwrap() = Some(Instant::now());
        latency.release_to_cleaned_ms = Some(ms(released));
        let prompt = cleaned.clone().unwrap_or_else(|| transcript.text.clone());
        *self.last_prompt.lock().unwrap() = Some(prompt.clone());

        // Save first: insertion problems (or a hung compositor) must never lose the prompt.
        let saving = Instant::now();
        let saved = if settings.history_enabled {
            let entry = Entry {
                id: 0,
                created_at: now_ms(),
                raw: transcript.text,
                cleaned,
                mode: if settings.cleanup_enabled {
                    "clean"
                } else {
                    "raw"
                }
                .into(),
                stt_model,
                // The model that wrote `cleaned`, or the one that was attempted.
                cleanup_model: cleanup_model.or_else(|| {
                    settings
                        .cleanup_enabled
                        .then(|| settings.cleanup_model.clone())
                }),
                error: cleanup_error.clone(),
                duration_ms: audio_ms as i64,
                latency_ms: released.elapsed().as_millis() as i64,
            };
            self.history
                .lock()
                .unwrap()
                .add(&entry)
                .map_err(|error| log::warn!("history write failed: {error:#}"))
                .ok()
        } else {
            None
        };
        latency.history_ms = ms(saving);

        let delivered = delivery::deliver(
            self.desktop.clone(),
            settings,
            &prompt,
            snapshot.target.as_ref(),
        )
        .await;
        let outcome = match delivered {
            Ok(delivered) => {
                latency.held_wait_ms = delivered.held_wait_ms;
                latency.insert_setup_ms = delivered.insert.setup_ms;
                latency.insert_keys_ms = delivered.insert.keys_ms;
                latency.clipboard_ms = delivered.insert.clipboard_ms;
                latency.delivery = delivered.delivery;
                latency.destination = delivered.destination;
                Outcome::Delivered {
                    message: delivered.message,
                    cleanup_failed: cleanup_error.is_some(),
                }
            }
            Err(error) => {
                latency.delivery = "failed".into();
                Outcome::Undelivered(error)
            }
        };
        latency.release_to_delivered_ms = ms(released);
        latency.log(prompt.chars().count());
        if let Some(id) = saved {
            let history = self.history.lock().unwrap();
            let _ = history.set_latency(id, latency.release_to_delivered_ms as i64);
            let _ = history.prune(settings.history_retention_days, now_ms());
        }
        *self.last_latency.lock().unwrap() = Some(latency);
        Ok(outcome)
    }
}

fn failure(error: &anyhow::Error) -> &'static str {
    Failure::of(error).map_or("error", Failure::name)
}

/// Per-dictation stage timings, logged as one `latency` line (`bench/run.py latency` summarizes them).
#[derive(Serialize, Clone, Default, Debug)]
pub struct Latency {
    pub hotkey_to_capture_ms: u64,
    /// `None` when no audio chunk arrived; never logged as zero.
    pub capture_to_first_audio_ms: Option<u64>,
    pub finish_ms: u64,
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
    /// `groq_direct` | `9router`, and the model that wrote the delivered text.
    cleanup_provider: &'static str,
    cleanup_model: String,
    /// The preferred cleaner's failure when the fallback model cleaned; empty otherwise.
    cleanup_fallback_reason: &'static str,
    /// Time lost to that failed attempt, part of `cleanup_total_ms`.
    cleanup_fallback_ms: u64,
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
    /// cleaned | cleaned_fallback (by the fallback model) | raw (cleanup off) |
    /// fallback_<failure> (raw delivered) | stt_<failure> | empty
    outcome: String,
    /// typed | pasted | pasted_terminal | copied_<reason> | failed
    delivery: String,
    /// confirmed | changed | unknown: the press-time window versus the window at delivery.
    destination: &'static str,
    /// streaming_deepgram | streaming_muse | streaming_scribe | batch_fallback | batch (streaming off, or a retry)
    stt_mode: &'static str,
    /// Why streaming produced no trusted transcript; empty unless `batch_fallback`.
    fallback_reason: &'static str,
    /// Streaming stages, in ms since key press; `None` when the stage never happened.
    token_ms: Option<u64>,
    connected_ms: Option<u64>,
    /// The provider accepts audio: socket open (Deepgram), handshake acknowledged (Muse).
    ready_ms: Option<u64>,
    first_sent_ms: Option<u64>,
    first_partial_ms: Option<u64>,
    first_final_ms: Option<u64>,
    released_ms: Option<u64>,
    last_sent_ms: Option<u64>,
    finalize_sent_ms: Option<u64>,
    /// Release → complete raw transcript, whichever path produced it.
    release_to_stt_ms: Option<u64>,
    /// Release → cleaned (or raw fallback) text ready for delivery.
    release_to_cleaned_ms: Option<u64>,
}

impl Latency {
    fn record_stream(&mut self, stats: &streaming::Stats, pressed: Instant, released: Instant) {
        let since = |at: Option<Instant>| {
            at.map(|at| at.saturating_duration_since(pressed).as_millis() as u64)
        };
        self.token_ms = since(stats.token);
        self.connected_ms = since(stats.connected);
        self.ready_ms = since(stats.ready);
        self.first_sent_ms = since(stats.first_sent);
        self.first_partial_ms = since(stats.first_partial);
        self.first_final_ms = since(stats.first_final);
        self.released_ms = since(Some(released));
        self.last_sent_ms = since(stats.last_sent);
        self.finalize_sent_ms = since(stats.finalize_sent);
    }

    fn log(&self, chars: usize) {
        let na = |value: Option<u64>| value.map_or("na".into(), |ms| ms.to_string());
        let first_audio = na(self.capture_to_first_audio_ms);
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
             cleanup_norm={} cleanup_total={} cleanup_attempts={} cleanup_provider={} cleanup_model={} \
             cleanup_fallback_reason={} cleanup_fallback={} history={} held_wait={} insert_setup={} \
             insert_keys={} clipboard={} untimed={} outcome={} delivery={} destination={} \
             stt_mode={} fallback_reason={} release_to_stt={} release_to_cleaned={} token={} \
             connected={} ready={} first_sent={} first_partial={} first_final={} released={} last_sent={} \
             finalize_sent={}",
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
            or_na(self.cleanup_provider),
            or_na(&self.cleanup_model),
            or_na(self.cleanup_fallback_reason),
            self.cleanup_fallback_ms,
            self.history_ms,
            self.held_wait_ms,
            self.insert_setup_ms,
            self.insert_keys_ms,
            self.clipboard_ms,
            self.release_to_delivered_ms.saturating_sub(timed),
            or_na(&self.outcome),
            or_na(&self.delivery),
            or_na(self.destination),
            or_na(self.stt_mode),
            or_na(self.fallback_reason),
            na(self.release_to_stt_ms),
            na(self.release_to_cleaned_ms),
            na(self.token_ms),
            na(self.connected_ms),
            na(self.ready_ms),
            na(self.first_sent_ms),
            na(self.first_partial_ms),
            na(self.first_final_ms),
            na(self.released_ms),
            na(self.last_sent_ms),
            na(self.finalize_sent_ms),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::fake::{window, Desktop as FakeDesktop};
    use crate::providers::{Cleaned, PromptCleaner, SpeechProvider};
    use anyhow::anyhow;
    use async_trait::async_trait;

    /// Batch STT answering from a script, one entry per call.
    #[derive(Clone, Default)]
    struct Stt {
        answers: Arc<Mutex<Vec<Result<String, &'static str>>>>,
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl SpeechProvider for Stt {
        async fn transcribe(&self, _: Vec<u8>, _: &str, _: Option<String>) -> Result<Transcript> {
            *self.calls.lock().unwrap() += 1;
            let answer = self.answers.lock().unwrap().remove(0);
            answer
                .map(|text| Transcript {
                    text,
                    timing: Default::default(),
                })
                .map_err(|message| anyhow!(message))
        }
    }

    struct Cleaner(Result<&'static str, &'static str>);

    /// Cleans through the fallback model after the preferred one was rate limited.
    struct FallbackCleaner;

    #[async_trait]
    impl PromptCleaner for FallbackCleaner {
        async fn clean(&self, _: &str, _: &[String], _: &str) -> Result<Cleaned> {
            Ok(Cleaned {
                text: "Fallback.".into(),
                timing: Default::default(),
                attempts: 1,
                provider: "9router",
                model: "claude-haiku-5-5".into(),
                fallback_reason: Some("rate_limited"),
                failed_ms: 40,
            })
        }
    }

    #[async_trait]
    impl PromptCleaner for Cleaner {
        async fn clean(&self, _: &str, _: &[String], _: &str) -> Result<Cleaned> {
            self.0
                .map(|text| Cleaned {
                    text: text.into(),
                    timing: Default::default(),
                    attempts: 1,
                    provider: "9router",
                    model: "fake-model".into(),
                    fallback_reason: None,
                    failed_ms: 0,
                })
                .map_err(|message| anyhow!(message))
        }
    }

    struct Rig {
        pipeline: Pipeline,
        stt: Stt,
        desktop: Arc<FakeDesktop>,
        _dir: tempfile::TempDir,
    }

    fn rig(
        stt: &[Result<&str, &'static str>],
        cleaner: Result<&'static str, &'static str>,
        desktop: FakeDesktop,
    ) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(&dir.path().join("history.db")).unwrap();
        let fake = Stt::default();
        *fake.answers.lock().unwrap() = stt.iter().map(|a| a.map(String::from)).collect();
        let shared = fake.clone();
        let connect: Connect = Box::new(move |_| {
            Ok(Providers {
                stt: Box::new(shared.clone()),
                cleaner: Box::new(Cleaner(cleaner)),
            })
        });
        let desktop = Arc::new(desktop);
        Rig {
            pipeline: Pipeline::new(history, connect, desktop.clone()),
            stt: fake,
            desktop,
            _dir: dir,
        }
    }

    fn speech() -> Vec<i16> {
        (0..16_000)
            .map(|i| if i % 2 == 0 { 3000 } else { -3000 })
            .collect()
    }

    fn recording(stream: Option<StreamSession>) -> Recording {
        Recording {
            samples: speech(),
            pressed: Some(Instant::now()),
            released: Instant::now(),
            held: Duration::from_secs(1),
            stream,
            latency: Latency::default(),
        }
    }

    /// Window tracking works and the press-time window is still active.
    fn same_window() -> (Snapshot, FakeDesktop) {
        let code = window("1", "code");
        let desktop = FakeDesktop {
            active: Some(Some(code.clone())),
            ..Default::default()
        };
        (Snapshot::take(Settings::default(), Some(code)), desktop)
    }

    fn history(rig: &Rig) -> Vec<Entry> {
        rig.pipeline.history.lock().unwrap().list(10).unwrap()
    }

    fn latency(rig: &Rig) -> Latency {
        rig.pipeline.last_latency.lock().unwrap().clone().unwrap()
    }

    fn typed(rig: &Rig) -> Vec<String> {
        rig.desktop
            .inserted
            .lock()
            .unwrap()
            .iter()
            .map(|(_, text)| text.clone())
            .collect()
    }

    #[tokio::test]
    async fn a_trusted_stream_skips_batch_stt_and_is_attributed() {
        let (snapshot, desktop) = same_window();
        let rig = rig(&[], Ok("Cleaned."), desktop);
        let stream = streaming::ended(Ok("streamed text"));
        let outcome = rig
            .pipeline
            .run(&snapshot, recording(Some(stream)), |_| {})
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            Outcome::Delivered {
                message: "Inserted",
                cleanup_failed: false
            }
        ));
        assert_eq!(*rig.stt.calls.lock().unwrap(), 0);
        let entry = &history(&rig)[0];
        assert_eq!(entry.raw, "streamed text");
        assert_eq!(entry.stt_model, "deepgram/nova-3");
        assert_eq!(latency(&rig).stt_mode, "streaming_deepgram");
        assert_eq!(typed(&rig), ["Cleaned."]);
    }

    #[tokio::test]
    async fn a_failed_stream_replays_the_recording_to_batch_stt() {
        let (snapshot, desktop) = same_window();
        let rig = rig(&[Ok("batch text")], Ok("Cleaned."), desktop);
        let stream = streaming::ended(Err("disconnected"));
        rig.pipeline
            .run(&snapshot, recording(Some(stream)), |_| {})
            .await
            .unwrap();
        assert_eq!(*rig.stt.calls.lock().unwrap(), 1);
        let latency = latency(&rig);
        assert_eq!(latency.stt_mode, "batch_fallback");
        assert_eq!(latency.fallback_reason, "disconnected");
        let entry = &history(&rig)[0];
        assert_eq!(entry.raw, "batch text");
        assert_eq!(entry.stt_model, Settings::default().stt_model);
    }

    #[tokio::test]
    async fn an_empty_stream_for_speech_is_not_trusted() {
        let (snapshot, desktop) = same_window();
        let rig = rig(&[Ok("batch text")], Ok("Cleaned."), desktop);
        let stream = streaming::ended(Ok(""));
        rig.pipeline
            .run(&snapshot, recording(Some(stream)), |_| {})
            .await
            .unwrap();
        assert_eq!(latency(&rig).fallback_reason, "empty");
        assert_eq!(history(&rig)[0].raw, "batch text");
    }

    #[tokio::test]
    async fn failed_stt_keeps_the_audio_and_a_retry_delivers_it() {
        let (snapshot, desktop) = same_window();
        let rig = rig(&[Err("timeout"), Ok("second try")], Ok("Cleaned."), desktop);
        let error = rig
            .pipeline
            .run(&snapshot, recording(None), |_| {})
            .await
            .unwrap_err();
        assert!(format!("{error}").contains("Retry last recording"));
        assert!(history(&rig).is_empty());

        let samples = rig.pipeline.take_failed_audio().expect("audio kept");
        assert_eq!(samples, speech());
        // A retry has no press-time window, so it must not type into whatever is active now.
        let retry = Snapshot::take(Settings::default(), None);
        let outcome = rig
            .pipeline
            .run(&retry, Recording::retry(samples), |_| {})
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Delivered { .. }));
        assert!(typed(&rig).is_empty());
        assert_eq!(*rig.desktop.copied.lock().unwrap(), ["Cleaned."]);
        assert_eq!(latency(&rig).delivery, "copied_no_target");
        assert!(rig.pipeline.take_failed_audio().is_none());
    }

    #[tokio::test]
    async fn a_cleanup_failure_delivers_the_raw_transcript() {
        let (snapshot, desktop) = same_window();
        let rig = rig(&[Ok("raw words")], Err("cleanup timed out"), desktop);
        let outcome = rig
            .pipeline
            .run(&snapshot, recording(None), |_| {})
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            Outcome::Delivered {
                cleanup_failed: true,
                ..
            }
        ));
        assert_eq!(typed(&rig), ["raw words"]);
        let entry = &history(&rig)[0];
        assert_eq!(entry.cleaned, None);
        assert_eq!(entry.error.as_deref(), Some("cleanup timed out"));
        assert_eq!(latency(&rig).outcome, "fallback_error");
    }

    #[tokio::test]
    async fn fallback_cleanup_is_attributed_to_the_fallback_model() {
        let (snapshot, desktop) = same_window();
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryStore::open(&dir.path().join("history.db")).unwrap();
        let stt = Stt::default();
        *stt.answers.lock().unwrap() = vec![Ok("raw words".into())];
        let connect: Connect = Box::new(move |_| {
            Ok(Providers {
                stt: Box::new(stt.clone()),
                cleaner: Box::new(FallbackCleaner),
            })
        });
        let pipeline = Pipeline::new(history, connect, Arc::new(desktop));
        pipeline
            .run(&snapshot, recording(None), |_| {})
            .await
            .unwrap();
        let latency = pipeline.last_latency.lock().unwrap().clone().unwrap();
        // Not counted as a preferred-model success.
        assert_eq!(latency.outcome, "cleaned_fallback");
        assert_eq!(latency.cleanup_fallback_reason, "rate_limited");
        assert_eq!(latency.cleanup_model, "claude-haiku-5-5");
        let entry = &pipeline.history.lock().unwrap().list(1).unwrap()[0];
        assert_eq!(entry.cleanup_model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(entry.cleaned.as_deref(), Some("Fallback."));
    }

    #[tokio::test]
    async fn the_prompt_survives_a_failed_delivery() {
        let (snapshot, mut desktop) = same_window();
        desktop.insert_fails = true;
        desktop.copy_fails = true;
        let rig = rig(&[Ok("raw words")], Ok("Cleaned."), desktop);
        let outcome = rig
            .pipeline
            .run(&snapshot, recording(None), |_| {})
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Undelivered(_)));
        assert_eq!(history(&rig)[0].cleaned.as_deref(), Some("Cleaned."));
        assert_eq!(
            rig.pipeline.last_prompt.lock().unwrap().as_deref(),
            Some("Cleaned.")
        );
        assert_eq!(latency(&rig).delivery, "failed");
    }

    #[tokio::test]
    async fn without_history_the_last_prompt_is_still_recoverable() {
        let (mut snapshot, desktop) = same_window();
        snapshot.settings.history_enabled = false;
        let rig = rig(&[Ok("raw words")], Ok("Cleaned."), desktop);
        rig.pipeline
            .run(&snapshot, recording(None), |_| {})
            .await
            .unwrap();
        assert!(history(&rig).is_empty());
        assert_eq!(
            rig.pipeline.last_prompt.lock().unwrap().as_deref(),
            Some("Cleaned.")
        );
    }

    #[tokio::test]
    async fn short_or_silent_recordings_send_nothing() {
        let (snapshot, desktop) = same_window();
        let rig = rig(&[], Ok("Cleaned."), desktop);
        let mut tap = recording(None);
        tap.held = Duration::from_millis(100);
        let outcome = rig.pipeline.run(&snapshot, tap, |_| {}).await.unwrap();
        assert!(matches!(outcome, Outcome::Ignored));
        let mut silent = recording(None);
        silent.samples = vec![0; 16_000];
        let outcome = rig.pipeline.run(&snapshot, silent, |_| {}).await.unwrap();
        assert!(matches!(outcome, Outcome::Ignored));
        assert_eq!(*rig.stt.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn empty_stt_reports_nothing_recognised() {
        let (snapshot, desktop) = same_window();
        let rig = rig(&[Ok("")], Ok("Cleaned."), desktop);
        let outcome = rig
            .pipeline
            .run(&snapshot, recording(None), |_| {})
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Empty));
        assert!(typed(&rig).is_empty());
    }

    #[test]
    fn stt_vocabulary_is_contextual_but_cleanup_keeps_the_list() {
        let settings = Settings::default();
        let titled = |app_id: &str, title: &str| Window {
            id: "1".into(),
            app_id: app_id.into(),
            title: title.into(),
        };
        let snapshot = Snapshot::take(settings.clone(), Some(titled("code", "main.rs - wispr")));
        assert!(snapshot.technical);
        assert_eq!(snapshot.vocabulary[0], "main.rs");
        assert!(snapshot.keyterms().contains(&"9Router".to_string()));
        let snapshot = Snapshot::take(settings.clone(), Some(titled("signal", "Shopping list")));
        assert!(snapshot.keyterms().is_empty(), "prose gets no STT keyterms");
        assert_eq!(
            snapshot.vocabulary, settings.vocabulary,
            "cleanup still filters the global list"
        );
        assert!(settings::relevant_terms(&snapshot.vocabulary, "buy milk and eggs").is_empty());
    }
}
