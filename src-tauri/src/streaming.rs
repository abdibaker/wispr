//! Live transcription: PCM streams from the capture thread straight to the provider while
//! the key is held. Audio never passes through 9Router.
//!
//! - Deepgram: 9Router mints a short-lived token. After `CloseStream`, Deepgram
//!   flushes the remaining results, sends `Metadata`, then closes; only that `Metadata`
//!   completes a session. `is_final`, `speech_final`, silence or a bare close never do.
//! - ElevenLabs Scribe (experimental): direct key in `xi-api-key`, manual commit. The release
//!   sends the last audio with `commit: true`; only the `committed_transcript` that follows
//!   completes a session.
//! - Meta Muse (default): the Meta key goes in the first JSON frame, not a header.
//!   After `endStream`, a push-to-talk session ends with one `transcript` event marked
//!   `final: true` and then a normal close (1000); anything else falls back to batch STT.
use crate::{
    audio,
    settings::{Settings, StreamingProvider},
};
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::{HeaderValue, StatusCode},
    Message,
};

/// Audio held while the token and socket are set up: 10 s of 16 kHz PCM16. Past it the
/// session fails and the batch fallback transcribes the retained recording instead.
pub const MAX_QUEUED_BYTES: usize = 10 * audio::RATE as usize * 2;
/// Deepgram closes a socket after 10 s without audio or KeepAlive.
const KEEP_ALIVE: Duration = Duration::from_secs(4);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LISTEN_URL: &str = "wss://api.deepgram.com/v1/listen";
pub const MUSE_URL: &str = "wss://api.meta.ai/v1/asr/realtime";
pub const MUSE_MODEL: &str = "muse-voice-transcribe-1.0";
/// Muse rejects a backlog of audio far ahead of real time (close 1008, documented as
/// "audio may not run more than 5 s ahead"), so early audio is replayed at most this far ahead.
const MUSE_MAX_LEAD: Duration = Duration::from_secs(3);
/// Nova-3 accepts up to 100 keyterms within 500 tokens; identifiers tokenize densely.
const MAX_KEYTERMS: usize = 50;
/// Muse fails the whole session (`provider_error`) on a keyword over 20 characters
/// (measured 2026-10), so longer terms are skipped rather than sent.
const MUSE_MAX_KEYWORD_CHARS: usize = 20;
pub const SCRIBE_URL: &str = "wss://api.elevenlabs.io/v1/speech-to-text/realtime";
pub const SCRIBE_MODEL: &str = "scribe_v2_realtime";
/// Documented realtime limits: at most 50 keyterms of at most 20 characters each.
const SCRIBE_MAX_KEYTERMS: usize = 50;
const SCRIBE_MAX_KEYTERM_CHARS: usize = 20;
/// Deepgram's flush after CloseStream normally takes a few hundred ms; past this the
/// session is untrusted and the retained recording goes to batch STT.
const FINALIZE_DEADLINE: Duration = Duration::from_secs(3);

/// Why streaming produced no trusted transcript; logged as `fallback_reason`.
#[derive(Debug)]
pub struct StreamError {
    pub reason: &'static str,
    message: String,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StreamError {}

fn fail(reason: &'static str, message: impl Into<String>) -> anyhow::Error {
    StreamError {
        reason,
        message: message.into(),
    }
    .into()
}

fn reason(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<StreamError>()
        .map_or("error", |e| e.reason)
}

/// When each stage of a session happened; `None` until it does.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub token: Option<Instant>,
    pub connected: Option<Instant>,
    /// The provider accepts audio: the socket opened (Deepgram) or the handshake was
    /// acknowledged (Muse).
    pub ready: Option<Instant>,
    pub first_sent: Option<Instant>,
    pub last_sent: Option<Instant>,
    pub finalize_sent: Option<Instant>,
    pub first_partial: Option<Instant>,
    pub first_final: Option<Instant>,
}

/// The capture side of a session. Pushing never blocks the audio thread; dropping it ends
/// the audio, which flushes the queue and finalizes.
pub struct AudioSink {
    sender: mpsc::UnboundedSender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    overflowed: Arc<AtomicBool>,
}

impl AudioSink {
    /// Queues one capture chunk (800 samples). The byte bound, not the message count,
    /// limits memory. On overflow the session stops: audio is never dropped from the middle.
    pub fn push(&self, samples: &[i16]) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let queued = self.queued.fetch_add(bytes.len(), Ordering::SeqCst) + bytes.len();
        if queued > MAX_QUEUED_BYTES {
            self.overflowed.store(true, Ordering::SeqCst);
            self.stopped.store(true, Ordering::SeqCst);
        } else if self.sender.send(bytes).is_err() {
            // The task already ended; `finish` reports why.
            self.stopped.store(true, Ordering::SeqCst);
        }
    }
}

/// A running session. Dropping it cancels: the socket closes without finalizing.
pub struct StreamSession {
    /// `streaming_deepgram` | `streaming_muse` | `streaming_scribe`
    mode: &'static str,
    /// Attribution for history, e.g. `deepgram/nova-3`.
    model: String,
    task: tauri::async_runtime::JoinHandle<Result<String>>,
    stopped: Arc<AtomicBool>,
    overflowed: Arc<AtomicBool>,
    stats: Arc<Mutex<Stats>>,
}

/// A transcript the provider confirmed complete, with the model that produced it.
#[derive(Debug, PartialEq)]
pub struct Streamed {
    pub text: String,
    pub model: String,
}

/// How a session ended. Only `transcript: Ok` may be used; any `Err` names the
/// `fallback_reason` and the caller replays the full recording to batch STT.
#[derive(Debug)]
pub struct Conclusion {
    pub transcript: std::result::Result<Streamed, &'static str>,
    /// `streaming_deepgram` | `streaming_muse` | `streaming_scribe`
    pub mode: &'static str,
    pub stats: Stats,
}

impl StreamSession {
    /// Ends the audio-side wait and decides whether the transcript can be trusted. Call once
    /// capture has stopped (the sink is dropped); only for audio the caller found not silent.
    pub async fn conclude(self) -> Conclusion {
        let stats = self.stats.clone();
        let mode = self.mode;
        let model = self.model.clone();
        let transcript = match self.finish(FINALIZE_DEADLINE).await {
            // Metadata arrived but nothing was finalized for audio that is not silent:
            // let batch STT have a go rather than report "nothing recognised".
            Ok(text) if text.is_empty() => Err("empty"),
            Ok(text) => Ok(Streamed { text, model }),
            Err(error) => Err(reason(&error)),
        };
        let stats = *stats.lock().unwrap();
        Conclusion {
            transcript,
            mode,
            stats,
        }
    }

    /// The complete transcript, once the provider confirms the session finished. Call after the
    /// sink is dropped; fails at `deadline` rather than returning partial text.
    async fn finish(mut self, deadline: Duration) -> Result<String> {
        if self.overflowed.load(Ordering::SeqCst) {
            return Err(fail("queue_overflow", "Streaming queue overflowed"));
        }
        match tokio::time::timeout(deadline, &mut self.task).await {
            Err(_) => Err(fail(
                "finalize_timeout",
                "Live transcription did not finish in time",
            )),
            Ok(Err(_)) => Err(fail("task", "Streaming task stopped")),
            Ok(Ok(result)) => result,
        }
    }
}

impl Drop for StreamSession {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

/// Starts the configured provider's session; audio is queued until its connection is ready.
pub fn start(
    client: reqwest::Client,
    settings: &Settings,
    keyterms: &[String],
) -> (StreamSession, AudioSink) {
    match settings.streaming_provider {
        StreamingProvider::Muse => MuseStreamingTranscriber::new(settings).start(keyterms),
        StreamingProvider::Scribe => ScribeStreamingTranscriber::new(settings).start(keyterms),
        StreamingProvider::Deepgram => {
            DeepgramStreamingTranscriber::new(client, settings).start(keyterms)
        }
    }
}

pub struct DeepgramStreamingTranscriber {
    pub client: reqwest::Client,
    /// 9Router base URL, which mints the token.
    pub endpoint: String,
    pub model: String,
    pub language: String,
}

impl DeepgramStreamingTranscriber {
    pub fn new(client: reqwest::Client, settings: &Settings) -> Self {
        Self {
            client,
            endpoint: settings.endpoint.clone(),
            model: settings.streaming_model.clone(),
            language: settings.language.clone(),
        }
    }

    pub fn start(&self, keyterms: &[String]) -> (StreamSession, AudioSink) {
        let url = listen_url(LISTEN_URL, &self.model, &self.language, keyterms);
        let token = Box::pin(token(self.client.clone(), self.endpoint.clone()));
        spawn(deepgram_label(&self.model), move |audio| {
            deepgram(token, url, audio)
        })
    }
}

fn deepgram_label(model: &str) -> (&'static str, String) {
    ("streaming_deepgram", format!("deepgram/{model}"))
}

fn muse_label() -> (&'static str, String) {
    ("streaming_muse", format!("meta/{MUSE_MODEL}"))
}

pub struct MuseStreamingTranscriber {
    pub language: String,
    /// Sends `languageBias` from `language`; off only for benchmarking the bare model.
    pub language_bias: bool,
}

impl MuseStreamingTranscriber {
    pub fn new(settings: &Settings) -> Self {
        Self {
            language: settings.language.clone(),
            language_bias: true,
        }
    }

    pub fn start(&self, keyterms: &[String]) -> (StreamSession, AudioSink) {
        let language = self.language_bias.then_some(self.language.as_str());
        let config = muse_config(language, keyterms);
        let key = keyring_key(
            crate::settings::secret::MUSE,
            "No Meta Muse API key. Open Settings → Speech.",
        );
        spawn(muse_label(), move |audio| {
            muse(key, MUSE_URL.into(), config, audio)
        })
    }
}

fn scribe_label() -> (&'static str, String) {
    ("streaming_scribe", format!("elevenlabs/{SCRIBE_MODEL}"))
}

/// Experimental: ElevenLabs Scribe v2 Realtime with the direct API key.
pub struct ScribeStreamingTranscriber {
    pub language: String,
    /// Audio per message; the capture fragment is 50 ms. Only for benchmarking.
    pub chunk_ms: u64,
}

impl ScribeStreamingTranscriber {
    pub fn new(settings: &Settings) -> Self {
        Self {
            language: settings.language.clone(),
            chunk_ms: 50,
        }
    }

    pub fn start(&self, keyterms: &[String]) -> (StreamSession, AudioSink) {
        let url = scribe_url(SCRIBE_URL, &self.language, keyterms);
        let chunk_bytes = (self.chunk_ms * audio::RATE as u64 / 1000 * 2) as usize;
        let key = keyring_key(
            crate::settings::secret::ELEVENLABS,
            "No ElevenLabs API key. Open Settings → Speech.",
        );
        spawn(scribe_label(), move |audio| {
            scribe(key, url, chunk_bytes, audio)
        })
    }
}

fn keyring_key(account: &'static str, missing: &'static str) -> TokenFuture {
    Box::pin(async move {
        tauri::async_runtime::spawn_blocking(move || crate::settings::secret::get_for(account))
            .await
            .map_err(|_| fail("auth", "Keyring read stopped"))??
            .ok_or_else(|| fail("auth", missing))
    })
}

type TokenFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;

/// The provider side of a session: queued capture audio and the stage timings.
struct Audio {
    receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    stats: Arc<Mutex<Stats>>,
}

impl Audio {
    /// The next queued chunk, or `None` once capture has ended and the queue is drained.
    async fn next(&mut self) -> Option<Vec<u8>> {
        let bytes = self.receiver.recv().await?;
        self.queued.fetch_sub(bytes.len(), Ordering::SeqCst);
        Some(bytes)
    }
}

/// Creates the queue first, then starts credential and connection work behind it.
fn spawn<F, Fut>((mode, model): (&'static str, String), run: F) -> (StreamSession, AudioSink)
where
    F: FnOnce(Audio) -> Fut,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    let (sender, receiver) = mpsc::unbounded_channel();
    let queued = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicBool::new(false));
    let overflowed = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Mutex::new(Stats::default()));
    let task = tauri::async_runtime::spawn(run(Audio {
        receiver,
        queued: queued.clone(),
        stats: stats.clone(),
    }));
    (
        StreamSession {
            mode,
            model,
            task,
            stopped: stopped.clone(),
            overflowed: overflowed.clone(),
            stats,
        },
        AudioSink {
            sender,
            queued,
            stopped,
            overflowed,
        },
    )
}

/// Exchanges the 9Router key for a short-lived Deepgram token. The permanent Deepgram key
/// stays in 9Router.
async fn token(client: reqwest::Client, endpoint: String) -> Result<String> {
    let key = tauri::async_runtime::spawn_blocking(crate::settings::secret::get)
        .await
        .map_err(|_| fail("token", "Keyring read stopped"))??
        .ok_or_else(|| fail("token", "No 9Router API key"))?;
    let response = client
        .post(format!("{}/realtime/token", endpoint.trim_end_matches('/')))
        .bearer_auth(key)
        .json(&serde_json::json!({ "provider": "deepgram" }))
        .timeout(TOKEN_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            let kind = if e.is_timeout() {
                "token_timeout"
            } else {
                "token"
            };
            fail(kind, "Cannot reach 9Router for a Deepgram token")
        })?;
    let status = response.status();
    if !status.is_success() {
        let kind = if status == StatusCode::TOO_MANY_REQUESTS {
            "rate_limited"
        } else {
            "token"
        };
        return Err(fail(
            kind,
            format!("9Router refused a Deepgram token ({status})"),
        ));
    }
    #[derive(Deserialize)]
    struct Grant {
        access_token: String,
    }
    let grant: Grant = response
        .json()
        .await
        .map_err(|_| fail("token", "Invalid Deepgram token response"))?;
    Ok(grant.access_token)
}

/// The listen URL. The token goes in a header, so the URL holds no secret.
fn listen_url(base: &str, model: &str, language: &str, keyterms: &[String]) -> String {
    let mut url = reqwest::Url::parse(base).expect("valid listen URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("model", model)
            .append_pair("encoding", "linear16")
            .append_pair("sample_rate", &audio::RATE.to_string())
            .append_pair("channels", "1")
            .append_pair("interim_results", "true")
            .append_pair("smart_format", "true")
            // Streaming smart_format alone leaves most numbers as words.
            .append_pair("numerals", "true")
            .append_pair(
                "language",
                if language.is_empty() || language == "auto" {
                    "multi"
                } else {
                    language
                },
            );
        for term in crate::settings::vocabulary_terms(keyterms)
            .into_iter()
            .take(MAX_KEYTERMS)
        {
            query.append_pair("keyterm", term);
        }
    }
    url.into()
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event {
    Results {
        #[serde(default)]
        is_final: bool,
        channel: Channel,
    },
    Metadata {},
    Error {},
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct Channel {
    alternatives: Vec<Alternative>,
}

#[derive(Deserialize)]
struct Alternative {
    transcript: String,
}

/// Finalized segments and the current provisional segment, which Deepgram may revise
/// freely until it marks the segment final.
#[derive(Default)]
struct Transcript {
    finals: Vec<String>,
    interim: String,
}

impl Transcript {
    fn apply(&mut self, text: &str, is_final: bool) {
        let text = text.trim();
        if is_final {
            self.interim.clear();
            if !text.is_empty() {
                self.finals.push(text.to_string());
            }
        } else {
            self.interim = text.to_string();
        }
    }

    /// The finalized text only: provisional words are never delivered.
    fn text(&self) -> String {
        self.finals.join(" ")
    }
}

fn mark(stats: &Mutex<Stats>, field: fn(&mut Stats) -> &mut Option<Instant>) {
    field(&mut stats.lock().unwrap()).get_or_insert_with(Instant::now);
}

async fn deepgram(token: TokenFuture, url: String, mut audio: Audio) -> Result<String> {
    let stats = audio.stats.clone();
    let token = token.await?;
    mark(&stats, |s| &mut s.token);
    let mut request = url
        .into_client_request()
        .map_err(|_| fail("connect", "Invalid Deepgram URL"))?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| fail("token", "Invalid Deepgram token"))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("Authorization", authorization);
    let (socket, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| fail("connect_timeout", "Deepgram connection timed out"))?
            .map_err(|error| match error {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    let status = response.status();
                    let kind = match status {
                        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
                        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "auth",
                        _ => "connect",
                    };
                    fail(kind, format!("Deepgram rejected the connection ({status})"))
                }
                _ => fail("connect", "Cannot connect to Deepgram"),
            })?;
    mark(&stats, |s| &mut s.connected);
    mark(&stats, |s| &mut s.ready);
    let (mut writer, mut reader) = socket.split();
    let closing = AtomicBool::new(false);
    let send = async {
        let mut keep_alive = tokio::time::interval(KEEP_ALIVE);
        keep_alive.tick().await;
        loop {
            let message = tokio::select! {
                chunk = audio.next() => match chunk {
                    Some(bytes) => Message::Binary(bytes.into()),
                    None => break,
                },
                _ = keep_alive.tick() => Message::text(r#"{"type":"KeepAlive"}"#),
            };
            let audio = message.is_binary();
            writer
                .send(message)
                .await
                .map_err(|_| fail("disconnected", "Deepgram connection lost while sending"))?;
            if audio {
                mark(&stats, |s| &mut s.first_sent);
                stats.lock().unwrap().last_sent = Some(Instant::now());
            }
        }
        closing.store(true, Ordering::SeqCst);
        writer
            .send(Message::text(r#"{"type":"CloseStream"}"#))
            .await
            .map_err(|_| fail("disconnected", "Deepgram connection lost while finalizing"))?;
        mark(&stats, |s| &mut s.finalize_sent);
        Ok::<(), anyhow::Error>(())
    };
    let receive = async {
        let mut transcript = Transcript::default();
        while let Some(message) = reader.next().await {
            let message = message.map_err(|_| fail("disconnected", "Deepgram connection lost"))?;
            match message {
                Message::Text(text) => match serde_json::from_str(&text)
                    .map_err(|_| fail("protocol", "Invalid Deepgram message"))?
                {
                    Event::Results { is_final, channel } => {
                        let text = channel
                            .alternatives
                            .first()
                            .map_or("", |a| a.transcript.as_str());
                        if !text.trim().is_empty() {
                            mark(&stats, |s| &mut s.first_partial);
                            if is_final {
                                mark(&stats, |s| &mut s.first_final);
                            }
                        }
                        transcript.apply(text, is_final);
                    }
                    Event::Metadata {} if closing.load(Ordering::SeqCst) => {
                        return Ok(transcript.text());
                    }
                    Event::Error {} => {
                        return Err(fail("provider_error", "Deepgram reported an error"))
                    }
                    _ => {}
                },
                Message::Close(_) => break,
                _ => {}
            }
        }
        Err(fail(
            "disconnected",
            "Deepgram closed before the transcript was complete",
        ))
    };
    let ((), text) = tokio::try_join!(send, receive)?;
    Ok(text)
}

/// The handshake minus the credential, which `muse` adds right before sending.
fn muse_config(language: Option<&str>, keyterms: &[String]) -> serde_json::Value {
    let mut config = serde_json::json!({
        "audioEncoding": "PCM_16KHZ",
        "model": MUSE_MODEL,
        "mode": "PUSH_TO_TALK",
        "partialMode": "CUMULATIVE",
        "emitAudioProgress": false,
    });
    let keywords: Vec<&str> = crate::settings::vocabulary_terms(keyterms)
        .into_iter()
        .filter(|t| t.chars().count() <= MUSE_MAX_KEYWORD_CHARS)
        .take(MAX_KEYTERMS)
        .collect();
    if !keywords.is_empty() {
        config["keywords"] = keywords.into();
    }
    // Muse takes language names. ponytail: common codes only; others auto-detect.
    let name = match language.unwrap_or_default() {
        "en" => "English",
        "fr" => "French",
        "es" => "Spanish",
        "de" => "German",
        "it" => "Italian",
        "pt" => "Portuguese",
        "nl" => "Dutch",
        _ => "",
    };
    if !name.is_empty() {
        config["languageBias"] = serde_json::json!([name]);
    }
    config
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum MuseEvent {
    Transcript {
        transcript: String,
        #[serde(rename = "final")]
        is_final: bool,
    },
    Error {
        #[serde(default, rename = "errorType")]
        error_type: String,
    },
    #[serde(other)]
    Other,
}

fn muse_error(error_type: &str) -> anyhow::Error {
    match error_type {
        "authentication_error" => fail("auth", "Muse authentication failed"),
        "rate_limit_error" => fail("rate_limited", "Muse rate limit reached"),
        _ => fail("provider_error", "Muse reported an error"),
    }
}

/// A close before the final transcript, classified by its documented code.
fn muse_close(
    frame: Option<tokio_tungstenite::tungstenite::protocol::CloseFrame>,
) -> anyhow::Error {
    match frame.map(|f| u16::from(f.code)) {
        Some(1013) => fail("rate_limited", "Muse rate limit reached"),
        Some(1008) => fail("policy", "Muse rejected the stream"),
        _ => fail("disconnected", "Muse stream ended before final transcript"),
    }
}

async fn muse(
    key: TokenFuture,
    url: String,
    mut config: serde_json::Value,
    mut audio: Audio,
) -> Result<String> {
    let stats = audio.stats.clone();
    let key = key.await?;
    let (socket, _) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url))
        .await
        .map_err(|_| fail("connect_timeout", "Muse connection timed out"))?
        .map_err(|error| match error {
            tokio_tungstenite::tungstenite::Error::Http(response)
                if response.status() == StatusCode::TOO_MANY_REQUESTS =>
            {
                fail("rate_limited", "Muse rate limit reached")
            }
            _ => fail("connect", "Muse connection failed"),
        })?;
    mark(&stats, |s| &mut s.connected);
    let (mut writer, mut reader) = socket.split();
    // The endpoint ignores the Authorization header: the key goes in the first frame.
    config["authorization"] = serde_json::json!({ "accessToken": format!("Bearer {key}") });
    let handshake = Message::text(config.to_string());
    drop(key);
    writer
        .send(handshake)
        .await
        .map_err(|_| fail("connect", "Muse connection failed"))?;
    // The acknowledgement is the only frame without a `type`; an error comes first instead.
    let acknowledged = tokio::time::timeout(CONNECT_TIMEOUT, async {
        while let Some(message) = reader.next().await {
            match message.map_err(|_| fail("connect", "Muse connection failed"))? {
                Message::Text(text) => {
                    let value: serde_json::Value = serde_json::from_str(&text)
                        .map_err(|_| fail("protocol", "Invalid Muse message"))?;
                    if value.get("type").is_none() && value.get("sessionId").is_some() {
                        return Ok(());
                    }
                    if let Ok(MuseEvent::Error { error_type }) = serde_json::from_value(value) {
                        return Err(muse_error(&error_type));
                    }
                }
                Message::Close(frame) => return Err(muse_close(frame)),
                _ => {}
            }
        }
        Err(fail("disconnected", "Muse closed during the handshake"))
    });
    acknowledged
        .await
        .map_err(|_| fail("connect_timeout", "Muse handshake timed out"))??;
    mark(&stats, |s| &mut s.ready);
    let closing = AtomicBool::new(false);
    let send = async {
        let mut sent_bytes = 0usize;
        while let Some(bytes) = audio.next().await {
            sent_bytes += bytes.len();
            writer
                .send(Message::Binary(bytes.into()))
                .await
                .map_err(|_| fail("disconnected", "Muse connection lost while sending"))?;
            let first = *stats
                .lock()
                .unwrap()
                .first_sent
                .get_or_insert_with(Instant::now);
            stats.lock().unwrap().last_sent = Some(Instant::now());
            // Pace replayed early audio; live capture already arrives in real time.
            let sent =
                Duration::from_millis((sent_bytes / (audio::RATE as usize * 2 / 1000)) as u64);
            if let Some(lead) = sent.checked_sub(first.elapsed() + MUSE_MAX_LEAD) {
                tokio::time::sleep(lead).await;
            }
        }
        closing.store(true, Ordering::SeqCst);
        writer
            .send(Message::text(r#"{"type":"endStream"}"#))
            .await
            .map_err(|_| fail("disconnected", "Muse connection lost while finalizing"))?;
        mark(&stats, |s| &mut s.finalize_sent);
        Ok::<(), anyhow::Error>(())
    };
    let receive = async {
        // Complete only on `final: true` followed by the documented normal close (1000):
        // an error or abnormal close after the final still means the session failed.
        let mut finished = None;
        while let Some(message) = reader.next().await {
            let message = message
                .map_err(|_| fail("disconnected", "Muse stream ended before final transcript"))?;
            match message {
                Message::Text(text) => match serde_json::from_str(&text)
                    .map_err(|_| fail("protocol", "Invalid Muse message"))?
                {
                    // CUMULATIVE partials replace each other and may revise earlier words,
                    // so only the final event's text is kept.
                    MuseEvent::Transcript {
                        transcript,
                        is_final,
                    } => {
                        if !transcript.trim().is_empty() {
                            mark(&stats, |s| &mut s.first_partial);
                        }
                        if is_final {
                            if !closing.load(Ordering::SeqCst) {
                                return Err(fail("protocol", "Muse finished before endStream"));
                            }
                            mark(&stats, |s| &mut s.first_final);
                            finished = Some(transcript.trim().to_string());
                        }
                    }
                    MuseEvent::Error { error_type } => return Err(muse_error(&error_type)),
                    MuseEvent::Other => {}
                },
                Message::Close(frame) => {
                    let normal = frame.as_ref().is_some_and(|f| u16::from(f.code) == 1000);
                    return match finished {
                        Some(text) if normal => Ok(text),
                        _ => Err(muse_close(frame)),
                    };
                }
                _ => {}
            }
        }
        Err(fail(
            "disconnected",
            "Muse stream ended before final transcript",
        ))
    };
    let ((), text) = tokio::try_join!(send, receive)?;
    Ok(text)
}

/// The realtime URL. The key goes in a header; `no_verbatim` stays off so the transcript
/// is raw speech and cleanup alone rewrites it.
fn scribe_url(base: &str, language: &str, keyterms: &[String]) -> String {
    let mut url = reqwest::Url::parse(base).expect("valid Scribe URL");
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("model_id", SCRIBE_MODEL)
            .append_pair("audio_format", "pcm_16000")
            .append_pair("commit_strategy", "manual")
            .append_pair("no_verbatim", "false");
        if !language.is_empty() && language != "auto" {
            query.append_pair("language_code", language);
        }
        // A longer term would be rejected; it stays available to cleanup.
        for term in crate::settings::vocabulary_terms(keyterms)
            .into_iter()
            .filter(|t| t.chars().count() <= SCRIBE_MAX_KEYTERM_CHARS)
            .take(SCRIBE_MAX_KEYTERMS)
        {
            query.append_pair("keyterms", term);
        }
    }
    url.into()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let n = group
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= group.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

fn scribe_chunk(bytes: &[u8], commit: bool) -> Message {
    Message::text(
        serde_json::json!({
            "message_type": "input_audio_chunk",
            "audio_base_64": base64(bytes),
            "commit": commit,
            "sample_rate": audio::RATE,
        })
        .to_string(),
    )
}

#[derive(Deserialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
enum ScribeEvent {
    SessionStarted {},
    PartialTranscript {
        text: String,
    },
    CommittedTranscript {
        text: String,
    },
    AuthError {},
    RateLimited {},
    QuotaExceeded {},
    CommitThrottled {},
    ResourceExhausted {},
    QueueOverflow {},
    /// `error`, `input_error`, `invalid_request`, `transcriber_error`, …
    #[serde(other)]
    Other,
}

fn scribe_error(message_type: &str) -> anyhow::Error {
    match message_type {
        "auth_error" | "unaccepted_terms" => fail("auth", "ElevenLabs authentication failed"),
        "rate_limited" | "quota_exceeded" | "commit_throttled" | "resource_exhausted"
        | "queue_overflow" => fail("rate_limited", "ElevenLabs rate limit reached"),
        _ => fail("provider_error", "ElevenLabs reported an error"),
    }
}

/// Parses one server frame; errors carry only their type, never the server's text.
fn scribe_event(text: &str) -> Result<ScribeEvent> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| fail("protocol", "Invalid ElevenLabs message"))?;
    let message_type = value["message_type"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let event: ScribeEvent = serde_json::from_value(value)
        .map_err(|_| fail("protocol", "Invalid ElevenLabs message"))?;
    match event {
        ScribeEvent::AuthError {}
        | ScribeEvent::RateLimited {}
        | ScribeEvent::QuotaExceeded {}
        | ScribeEvent::CommitThrottled {}
        | ScribeEvent::ResourceExhausted {}
        | ScribeEvent::QueueOverflow {} => Err(scribe_error(&message_type)),
        // Only error frames have an `error` field; warnings and extra events are ignored.
        ScribeEvent::Other
            if message_type.ends_with("error") || message_type == "invalid_request" =>
        {
            Err(scribe_error(&message_type))
        }
        event => Ok(event),
    }
}

/// Streams with manual commit. After the audio ends, one commit frame closes the segment and
/// the session completes on the next `committed_transcript`; partials are never delivered.
/// Segments the server commits on its own (~36 s of audio) are kept in order.
async fn scribe(
    key: TokenFuture,
    url: String,
    chunk_bytes: usize,
    mut audio: Audio,
) -> Result<String> {
    let stats = audio.stats.clone();
    let key = key.await?;
    let mut request = url
        .into_client_request()
        .map_err(|_| fail("connect", "Invalid ElevenLabs URL"))?;
    let mut header =
        HeaderValue::from_str(&key).map_err(|_| fail("auth", "Invalid ElevenLabs key"))?;
    header.set_sensitive(true);
    request.headers_mut().insert("xi-api-key", header);
    drop(key);
    let (socket, _) =
        tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
            .await
            .map_err(|_| fail("connect_timeout", "ElevenLabs connection timed out"))?
            .map_err(|error| match error {
                tokio_tungstenite::tungstenite::Error::Http(response) => {
                    let status = response.status();
                    let kind = match status {
                        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
                        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "auth",
                        _ => "connect",
                    };
                    fail(
                        kind,
                        format!("ElevenLabs rejected the connection ({status})"),
                    )
                }
                _ => fail("connect", "Cannot connect to ElevenLabs"),
            })?;
    mark(&stats, |s| &mut s.connected);
    let (mut writer, mut reader) = socket.split();
    let started = tokio::time::timeout(CONNECT_TIMEOUT, async {
        while let Some(message) = reader.next().await {
            match message.map_err(|_| fail("connect", "ElevenLabs connection failed"))? {
                Message::Text(text) => {
                    if let ScribeEvent::SessionStarted {} = scribe_event(&text)? {
                        return Ok(());
                    }
                }
                Message::Close(_) => {
                    return Err(fail(
                        "disconnected",
                        "ElevenLabs closed during the handshake",
                    ))
                }
                _ => {}
            }
        }
        Err(fail(
            "disconnected",
            "ElevenLabs closed during the handshake",
        ))
    });
    started
        .await
        .map_err(|_| fail("connect_timeout", "ElevenLabs handshake timed out"))??;
    mark(&stats, |s| &mut s.ready);
    let committing = AtomicBool::new(false);
    let send = async {
        let mut pending = Vec::with_capacity(chunk_bytes);
        let mut ended = false;
        while !ended {
            match audio.next().await {
                Some(bytes) => pending.extend_from_slice(&bytes),
                None => ended = true,
            }
            if pending.len() < chunk_bytes && !ended {
                continue;
            }
            // The last frame carries the remaining audio and the commit together.
            if ended {
                committing.store(true, Ordering::SeqCst);
            }
            writer
                .send(scribe_chunk(&pending, ended))
                .await
                .map_err(|_| fail("disconnected", "ElevenLabs connection lost while sending"))?;
            if !pending.is_empty() {
                mark(&stats, |s| &mut s.first_sent);
                stats.lock().unwrap().last_sent = Some(Instant::now());
            }
            pending.clear();
        }
        mark(&stats, |s| &mut s.finalize_sent);
        Ok::<(), anyhow::Error>(())
    };
    let receive = async {
        let mut segments: Vec<String> = Vec::new();
        while let Some(message) = reader.next().await {
            let message = message
                .map_err(|_| fail("disconnected", "ElevenLabs stream ended before the commit"))?;
            match message {
                Message::Text(text) => match scribe_event(&text)? {
                    ScribeEvent::PartialTranscript { text } => {
                        if !text.trim().is_empty() {
                            mark(&stats, |s| &mut s.first_partial);
                        }
                    }
                    ScribeEvent::CommittedTranscript { text } => {
                        let text = text.trim();
                        if !text.is_empty() {
                            segments.push(text.to_string());
                        }
                        if committing.load(Ordering::SeqCst) {
                            mark(&stats, |s| &mut s.first_final);
                            return Ok(segments.join(" "));
                        }
                    }
                    _ => {}
                },
                Message::Close(_) => break,
                _ => {}
            }
        }
        Err(fail(
            "disconnected",
            "ElevenLabs stream ended before the committed transcript",
        ))
    };
    // ponytail: a server auto-commit (~36 s of audio) landing between the flag and our
    // commit's reply would end the session one segment early; push-to-talk rarely nears it.
    let ((), text) = tokio::try_join!(send, receive)?;
    Ok(text)
}

/// Checks the stored ElevenLabs key: a session must start (no audio is sent).
pub async fn scribe_check() -> Result<()> {
    let (session, sink) = ScribeStreamingTranscriber {
        language: String::new(),
        chunk_ms: 50,
    }
    .start(&[]);
    let stats = session.stats.clone();
    drop(sink);
    match session.finish(Duration::from_secs(10)).await {
        Ok(_) => Ok(()),
        Err(_) if stats.lock().unwrap().ready.is_some() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Checks the stored Meta key with a handshake that sends no audio.
pub async fn muse_check() -> Result<()> {
    let (session, sink) = MuseStreamingTranscriber {
        language: String::new(),
        language_bias: false,
    }
    .start(&[]);
    let stats = session.stats.clone();
    drop(sink);
    match session.finish(Duration::from_secs(10)).await {
        Ok(_) => Ok(()),
        // A session with no audio may legitimately end without a final transcript.
        Err(_) if stats.lock().unwrap().ready.is_some() => Ok(()),
        Err(error) => Err(error),
    }
}

/// A session that already ended with `result`, for tests of callers.
#[cfg(test)]
pub fn ended(result: std::result::Result<&'static str, &'static str>) -> StreamSession {
    let (session, _sink) = spawn(deepgram_label("nova-3"), move |_audio| async move {
        result
            .map(String::from)
            .map_err(|reason| fail(reason, reason))
    });
    session
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    fn result(text: &str, is_final: bool) -> Message {
        Message::text(
            serde_json::json!({
                "type": "Results",
                "is_final": is_final,
                "speech_final": is_final,
                "channel": {"alternatives": [{"transcript": text}]},
            })
            .to_string(),
        )
    }

    fn metadata() -> Message {
        Message::text(r#"{"type":"Metadata","request_id":"r"}"#)
    }

    fn token_after(delay: Duration) -> TokenFuture {
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok("test-token".to_string())
        })
    }

    fn dg(token: TokenFuture, url: String) -> (StreamSession, AudioSink) {
        spawn(deepgram_label("nova-3"), move |audio| {
            deepgram(token, url, audio)
        })
    }

    fn chunk(value: i16) -> Vec<i16> {
        vec![value; 800]
    }

    /// A mock Deepgram that checks the Bearer token and hands the socket to `script`.
    async fn server<F, Fut>(script: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: FnOnce(Socket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/listen", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            // The error type is fixed by tungstenite's callback signature.
            #[allow(clippy::result_large_err)]
            let check = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                assert_eq!(request.headers()["Authorization"], "Bearer test-token");
                assert!(!request.uri().to_string().contains("test-token"));
                Ok(response)
            };
            let socket = tokio_tungstenite::accept_hdr_async(connection, check)
                .await
                .unwrap();
            script(socket).await;
        });
        (url, handle)
    }

    /// Reads until CloseStream, returning all audio bytes received before it.
    async fn audio_until_close(socket: &mut Socket) -> Vec<u8> {
        let mut audio = Vec::new();
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Binary(bytes) => audio.extend_from_slice(&bytes),
                Message::Text(text) if text.contains("CloseStream") => return audio,
                _ => {}
            }
        }
        panic!("missing CloseStream");
    }

    fn samples_of(audio: &[u8]) -> Vec<i16> {
        audio
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect()
    }

    #[tokio::test]
    async fn audio_before_a_slow_token_and_connection_is_replayed_in_order() {
        let (url, server) = server(|mut socket| async move {
            let audio = audio_until_close(&mut socket).await;
            let expected: Vec<i16> = (0..40).flat_map(|i| chunk(i as i16)).collect();
            assert_eq!(samples_of(&audio), expected);
            socket.send(result("hello world", true)).await.unwrap();
            socket.send(metadata()).await.unwrap();
        })
        .await;
        let (session, sink) = dg(token_after(Duration::from_millis(300)), url);
        for i in 0..40 {
            sink.push(&chunk(i));
        }
        assert!(session.stats.lock().unwrap().token.is_none());
        drop(sink);
        let text = session.finish(Duration::from_secs(3)).await.unwrap();
        assert_eq!(text, "hello world");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn interim_revisions_are_never_delivered_and_finals_join_in_order() {
        let (url, server) = server(|mut socket| async move {
            socket.next().await;
            socket.send(result("deploy to", false)).await.unwrap();
            socket.send(result("deploy tomorrow", false)).await.unwrap();
            socket.send(result("Deploy tomorrow.", true)).await.unwrap();
            socket.send(result("no change", false)).await.unwrap();
            // A long pause: the server stays quiet, the client keeps the session open.
            audio_until_close(&mut socket).await;
            socket
                .send(result("No, change that to Monday.", true))
                .await
                .unwrap();
            socket.send(result("", true)).await.unwrap();
            socket.send(metadata()).await.unwrap();
        })
        .await;
        let (session, sink) = dg(token_after(Duration::ZERO), url);
        sink.push(&chunk(1));
        tokio::time::sleep(Duration::from_millis(100)).await;
        for _ in 0..200 {
            sink.push(&chunk(2));
        }
        drop(sink);
        let text = session.finish(Duration::from_secs(3)).await.unwrap();
        assert_eq!(text, "Deploy tomorrow. No, change that to Monday.");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn the_tail_pushed_just_before_release_is_sent_before_finalize() {
        let (url, server) = server(|mut socket| async move {
            let audio = audio_until_close(&mut socket).await;
            assert_eq!(*samples_of(&audio).last().unwrap(), 7);
            socket.send(result("tail", true)).await.unwrap();
            socket.send(metadata()).await.unwrap();
        })
        .await;
        let (session, sink) = dg(token_after(Duration::ZERO), url);
        sink.push(&chunk(1));
        sink.push(&chunk(7));
        drop(sink);
        assert_eq!(
            session.finish(Duration::from_secs(3)).await.unwrap(),
            "tail"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_before_closestream_does_not_complete_the_session() {
        let (url, server) = server(|mut socket| async move {
            socket.next().await;
            socket.send(result("early", true)).await.unwrap();
            socket.send(metadata()).await.unwrap();
            audio_until_close(&mut socket).await;
            socket.send(result("late", true)).await.unwrap();
            socket.send(metadata()).await.unwrap();
        })
        .await;
        let (session, sink) = dg(token_after(Duration::ZERO), url);
        sink.push(&chunk(1));
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(sink);
        assert_eq!(
            session.finish(Duration::from_secs(3)).await.unwrap(),
            "early late"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_close_without_terminal_metadata_fails() {
        for frame in [
            None,
            Some(CloseFrame {
                code: 1000.into(),
                reason: "".into(),
            }),
        ] {
            let (url, server) = server(move |mut socket| async move {
                audio_until_close(&mut socket).await;
                socket.send(result("partial", true)).await.unwrap();
                socket.send(Message::Close(frame)).await.unwrap();
            })
            .await;
            let (session, sink) = dg(token_after(Duration::ZERO), url);
            sink.push(&chunk(1));
            drop(sink);
            let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
            assert_eq!(reason(&error), "disconnected");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_mid_stream_disconnect_fails_without_partial_text() {
        let (url, server) = server(|mut socket| async move {
            socket.next().await;
            socket.send(result("half a", true)).await.unwrap();
            drop(socket);
        })
        .await;
        let (session, sink) = dg(token_after(Duration::ZERO), url);
        sink.push(&chunk(1));
        server.await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        sink.push(&chunk(2));
        drop(sink);
        let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
        assert_eq!(reason(&error), "disconnected");
    }

    #[tokio::test]
    async fn a_silent_server_hits_the_finalize_deadline() {
        let (url, server) = server(|mut socket| async move {
            audio_until_close(&mut socket).await;
            socket.send(result("partial", true)).await.unwrap();
            // Never sends Metadata; waits for the client to give up.
            while socket.next().await.is_some() {}
        })
        .await;
        let (session, sink) = dg(token_after(Duration::ZERO), url);
        sink.push(&chunk(1));
        drop(sink);
        let started = Instant::now();
        let error = session
            .finish(Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!(reason(&error), "finalize_timeout");
        assert!(started.elapsed() < Duration::from_secs(1));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn token_failure_is_reported_as_the_fallback_reason() {
        let token: TokenFuture = Box::pin(async { Err(fail("rate_limited", "429")) });
        let (session, sink) = dg(token, "ws://127.0.0.1:9/".into());
        sink.push(&chunk(1));
        drop(sink);
        let error = session.finish(Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(reason(&error), "rate_limited");
    }

    #[tokio::test]
    async fn rejected_and_unreachable_connections_fail_setup() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut connection, _) = listener.accept().await.unwrap();
            connection
                .write_all(b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let (session, sink) = dg(token_after(Duration::ZERO), format!("ws://{address}/"));
        drop(sink);
        let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
        assert_eq!(reason(&error), "rate_limited");
        server.await.unwrap();

        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = unused.local_addr().unwrap();
        drop(unused);
        let (session, sink) = dg(token_after(Duration::ZERO), format!("ws://{address}/"));
        drop(sink);
        let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
        assert_eq!(reason(&error), "connect");
    }

    #[tokio::test]
    async fn overflow_stops_the_session_instead_of_dropping_middle_audio() {
        let (session, sink) = dg(Box::pin(std::future::pending()), "ws://127.0.0.1:9/".into());
        let chunks = MAX_QUEUED_BYTES / 1600;
        for _ in 0..chunks {
            sink.push(&chunk(1));
        }
        assert!(!sink.stopped.load(Ordering::SeqCst));
        sink.push(&chunk(1));
        assert!(sink.stopped.load(Ordering::SeqCst));
        drop(sink);
        let error = session.finish(Duration::from_secs(1)).await.unwrap_err();
        assert_eq!(reason(&error), "queue_overflow");
    }

    #[tokio::test]
    async fn cancelling_closes_the_socket_and_stops_accepting_audio() {
        let (heard_sender, heard) = tokio::sync::oneshot::channel();
        let (url, server) = server(|mut socket| async move {
            assert!(matches!(socket.next().await, Some(Ok(Message::Binary(_)))));
            heard_sender.send(()).unwrap();
            loop {
                match socket.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(Message::Text(text))) => assert!(!text.contains("CloseStream")),
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        let (session, sink) = dg(token_after(Duration::ZERO), url);
        sink.push(&chunk(1));
        heard.await.unwrap();
        drop(session);
        sink.push(&chunk(2));
        assert!(sink.stopped.load(Ordering::SeqCst));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancelling_before_the_token_arrives_never_connects() {
        let (session, sink) = dg(Box::pin(std::future::pending()), "ws://127.0.0.1:9/".into());
        sink.push(&chunk(1));
        let stats = session.stats.clone();
        drop(session);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(stats.lock().unwrap().connected.is_none());
    }

    #[test]
    fn listen_url_carries_keyterms_but_no_secret() {
        let terms = vec![
            "Cargo.toml".into(),
            "@tanstack/react-query".into(),
            "cargo.toml".into(),
        ];
        let url = listen_url(LISTEN_URL, "nova-3", "en", &terms);
        let parsed = reqwest::Url::parse(&url).unwrap();
        let keyterms: Vec<_> = parsed
            .query_pairs()
            .filter(|(k, _)| k == "keyterm")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(keyterms, ["Cargo.toml", "@tanstack/react-query"]);
        assert!(url.contains("interim_results=true"));
        assert!(url.contains("encoding=linear16") && url.contains("sample_rate=16000"));
        assert!(listen_url(LISTEN_URL, "nova-3", "auto", &[]).contains("language=multi"));
        assert!(!listen_url(LISTEN_URL, "nova-3", "en", &[]).contains("keyterm"));
        let many: Vec<String> = (0..80).map(|i| format!("term{i}")).collect();
        assert_eq!(
            listen_url(LISTEN_URL, "nova-3", "en", &many)
                .matches("keyterm")
                .count(),
            MAX_KEYTERMS
        );
    }

    // ---------- Muse ----------

    const MUSE_KEY: &str = "meta-secret-key";

    fn muse_key_after(delay: Duration) -> TokenFuture {
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(MUSE_KEY.to_string())
        })
    }

    fn muse_session(
        key: TokenFuture,
        url: String,
        keyterms: &[String],
    ) -> (StreamSession, AudioSink) {
        let config = muse_config(Some("en"), keyterms);
        spawn(muse_label(), move |audio| muse(key, url, config, audio))
    }

    fn muse_transcript(text: &str, is_final: bool) -> Message {
        Message::text(
            serde_json::json!({"type": "transcript", "transcript": text, "final": is_final, "audioProcessedMs": 100})
                .to_string(),
        )
    }

    fn close(code: u16) -> Message {
        Message::Close(Some(CloseFrame {
            code: code.into(),
            reason: "".into(),
        }))
    }

    /// A mock Muse: checks that the key arrives only in the handshake frame, hands the
    /// handshake to `ack` (which sends the reply), then runs `script`.
    async fn muse_server<A, F, Fut>(ack: A, script: F) -> (String, tokio::task::JoinHandle<()>)
    where
        A: FnOnce(serde_json::Value) -> Option<Message> + Send + 'static,
        F: FnOnce(Socket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/asr/realtime", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            #[allow(clippy::result_large_err)]
            let check = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                assert!(request.headers().get("Authorization").is_none());
                assert!(!request.uri().to_string().contains(MUSE_KEY));
                Ok(response)
            };
            let mut socket = tokio_tungstenite::accept_hdr_async(connection, check)
                .await
                .unwrap();
            let Some(Ok(Message::Text(text))) = socket.next().await else {
                panic!("missing handshake");
            };
            let handshake: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                handshake["authorization"]["accessToken"],
                format!("Bearer {MUSE_KEY}")
            );
            let reply = ack(handshake);
            let rejected = reply
                .as_ref()
                .is_some_and(|m| !m.to_text().unwrap().contains("sessionId\":\"s1"));
            if let Some(reply) = reply {
                socket.send(reply).await.unwrap();
            }
            if !rejected {
                script(socket).await;
            }
        });
        (url, handle)
    }

    fn accept(_: serde_json::Value) -> Option<Message> {
        Some(Message::text(r#"{"sessionId":"s1"}"#))
    }

    /// Reads until endStream, returning all audio bytes received before it.
    async fn audio_until_end(socket: &mut Socket) -> Vec<u8> {
        let mut audio = Vec::new();
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Binary(bytes) => audio.extend_from_slice(&bytes),
                Message::Text(text) => {
                    assert_eq!(text.as_str(), r#"{"type":"endStream"}"#);
                    return audio;
                }
                _ => {}
            }
        }
        panic!("missing endStream");
    }

    #[test]
    fn muse_handshake_matches_the_documented_schema() {
        let config = muse_config(Some("en"), &[]);
        assert_eq!(config["audioEncoding"], "PCM_16KHZ");
        assert_eq!(config["model"], "muse-voice-transcribe-1.0");
        assert_eq!(config["mode"], "PUSH_TO_TALK");
        assert_eq!(config["partialMode"], "CUMULATIVE");
        assert_eq!(config["languageBias"], serde_json::json!(["English"]));
        assert!(config.get("keywords").is_none(), "prose sends no keywords");
        assert!(
            config.get("authorization").is_none(),
            "the key is added only when sending"
        );
        let auto = muse_config(Some("auto"), &[]);
        assert!(auto.get("languageBias").is_none());
        assert!(muse_config(None, &[]).get("languageBias").is_none());
        let terms: Vec<String> = [
            "9Router",
            "T3 Code",
            "whisper-large-v3-turbo",
            "pnpm",
            "9router",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            muse_config(Some("en"), &terms)["keywords"],
            serde_json::json!(["9Router", "T3 Code", "pnpm"])
        );
    }

    #[tokio::test]
    async fn muse_replays_early_audio_in_order_and_keeps_only_the_final_text() {
        let (url, server) = muse_server(
            |handshake| {
                assert_eq!(handshake["keywords"], serde_json::json!(["T3 Code"]));
                accept(handshake)
            },
            |mut socket| async move {
                let audio = audio_until_end(&mut socket).await;
                let expected: Vec<i16> = (0..40).flat_map(|i| chunk(i as i16)).collect();
                assert_eq!(samples_of(&audio), expected);
                for partial in [
                    "deploy to",
                    "deploy tomorrow no",
                    "Deploy tomorrow, no, change",
                ] {
                    socket.send(muse_transcript(partial, false)).await.unwrap();
                }
                socket
                    .send(muse_transcript(
                        "Deploy tomorrow, no, change that to Monday.",
                        true,
                    ))
                    .await
                    .unwrap();
                socket.send(close(1000)).await.unwrap();
            },
        )
        .await;
        let (session, sink) = muse_session(
            muse_key_after(Duration::from_millis(300)),
            url,
            &["T3 Code".into()],
        );
        for i in 0..40 {
            sink.push(&chunk(i));
        }
        assert!(session.stats.lock().unwrap().ready.is_none());
        drop(sink);
        let stats = session.stats.clone();
        let text = session.finish(Duration::from_secs(3)).await.unwrap();
        assert_eq!(text, "Deploy tomorrow, no, change that to Monday.");
        let stats = *stats.lock().unwrap();
        assert!(stats.ready.unwrap() <= stats.first_sent.unwrap());
        assert!(stats.finalize_sent.unwrap() <= stats.first_final.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn muse_close_without_final_fails_without_partial_text() {
        for code in [Some(1000), None] {
            let (url, server) = muse_server(accept, move |mut socket| async move {
                audio_until_end(&mut socket).await;
                socket
                    .send(muse_transcript("partial words", false))
                    .await
                    .unwrap();
                match code {
                    Some(code) => socket.send(close(code)).await.unwrap(),
                    None => drop(socket),
                }
            })
            .await;
            let (session, sink) = muse_session(muse_key_after(Duration::ZERO), url, &[]);
            sink.push(&chunk(1));
            drop(sink);
            let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
            assert_eq!(reason(&error), "disconnected");
            assert_eq!(
                error.to_string(),
                "Muse stream ended before final transcript"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_muse_final_followed_by_an_abnormal_end_is_not_trusted() {
        let endings = [
            Some(close(1011)),
            Some(Message::text(
                r#"{"type":"error","message":"x","sessionId":"s1"}"#,
            )),
            None,
        ];
        for ending in endings {
            let (url, server) = muse_server(accept, move |mut socket| async move {
                audio_until_end(&mut socket).await;
                socket
                    .send(muse_transcript("complete text", true))
                    .await
                    .unwrap();
                match ending {
                    Some(message) => socket.send(message).await.unwrap(),
                    None => drop(socket),
                }
            })
            .await;
            let (session, sink) = muse_session(muse_key_after(Duration::ZERO), url, &[]);
            sink.push(&chunk(1));
            drop(sink);
            assert!(session.finish(Duration::from_secs(3)).await.is_err());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_muse_final_before_endstream_is_not_trusted() {
        let (url, server) = muse_server(accept, |mut socket| async move {
            socket.next().await;
            socket.send(muse_transcript("early", true)).await.unwrap();
            while socket.next().await.is_some() {}
        })
        .await;
        let (session, sink) = muse_session(muse_key_after(Duration::ZERO), url, &[]);
        sink.push(&chunk(1));
        let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
        assert_eq!(reason(&error), "protocol");
        drop(sink);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn muse_auth_and_rate_limit_failures_are_classified_without_the_key() {
        let cases: [(Message, &str, &str); 3] = [
            (
                Message::text(
                    r#"{"type":"error","message":"Unauthorized: meta-secret-key","sessionId":"","errorType":"authentication_error","errorCode":"invalid_api_key"}"#,
                ),
                "auth",
                "Muse authentication failed",
            ),
            (close(1013), "rate_limited", "Muse rate limit reached"),
            (close(1008), "policy", "Muse rejected the stream"),
        ];
        for (reply, expected, message) in cases {
            let (url, server) = muse_server(move |_| Some(reply), |_| async {}).await;
            let (session, sink) = muse_session(muse_key_after(Duration::ZERO), url, &[]);
            sink.push(&chunk(1));
            drop(sink);
            let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
            assert_eq!(reason(&error), expected);
            assert_eq!(error.to_string(), message);
            assert!(!format!("{error:?}").contains(MUSE_KEY));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_silent_muse_server_hits_the_finalize_deadline() {
        let (url, server) = muse_server(accept, |mut socket| async move {
            audio_until_end(&mut socket).await;
            while socket.next().await.is_some() {}
        })
        .await;
        let (session, sink) = muse_session(muse_key_after(Duration::ZERO), url, &[]);
        sink.push(&chunk(1));
        drop(sink);
        let error = session
            .finish(Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!(reason(&error), "finalize_timeout");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_muse_closes_the_socket_without_endstream() {
        let (heard_sender, heard) = tokio::sync::oneshot::channel();
        let (url, server) = muse_server(accept, |mut socket| async move {
            assert!(matches!(socket.next().await, Some(Ok(Message::Binary(_)))));
            heard_sender.send(()).unwrap();
            loop {
                match socket.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(Message::Text(text))) => panic!("unexpected {text}"),
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        let (session, sink) = muse_session(muse_key_after(Duration::ZERO), url, &[]);
        sink.push(&chunk(1));
        heard.await.unwrap();
        drop(session);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn muse_paces_a_long_early_backlog_near_real_time() {
        let (url, server) = muse_server(accept, |mut socket| async move {
            audio_until_end(&mut socket).await;
            socket.send(muse_transcript("done", true)).await.unwrap();
            socket.send(close(1000)).await.unwrap();
        })
        .await;
        // 4.5 s of audio queued before the handshake: 3 s goes at once, the rest is paced.
        let (session, sink) = muse_session(muse_key_after(Duration::ZERO), url, &[]);
        for _ in 0..90 {
            sink.push(&chunk(1));
        }
        drop(sink);
        let started = Instant::now();
        assert_eq!(
            session.finish(Duration::from_secs(5)).await.unwrap(),
            "done"
        );
        assert!(started.elapsed() >= Duration::from_millis(1300));
        server.await.unwrap();
    }
}

#[cfg(test)]
mod scribe_tests {
    use super::*;
    use tokio::net::TcpListener;

    type Socket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;
    const KEY: &str = "sk_scribe-secret";

    fn key_after(delay: Duration) -> TokenFuture {
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(KEY.to_string())
        })
    }

    fn session(url: String, chunk_ms: u64) -> (StreamSession, AudioSink) {
        let chunk_bytes = (chunk_ms * audio::RATE as u64 / 1000 * 2) as usize;
        spawn(scribe_label(), move |audio| {
            scribe(
                key_after(Duration::from_millis(200)),
                url,
                chunk_bytes,
                audio,
            )
        })
    }

    fn event(json: serde_json::Value) -> Message {
        Message::text(json.to_string())
    }

    fn partial(text: &str) -> Message {
        event(serde_json::json!({"message_type": "partial_transcript", "text": text}))
    }

    fn committed(text: &str) -> Message {
        event(serde_json::json!({"message_type": "committed_transcript", "text": text}))
    }

    fn chunk(value: i16) -> Vec<i16> {
        vec![value; 800]
    }

    /// A mock Scribe: checks the key header and that the URL holds no secret, sends
    /// `session_started` unless `start` is false, then runs `script`.
    async fn server<F, Fut>(start: bool, script: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: FnOnce(Socket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = scribe_url(
            &format!(
                "ws://{}/v1/speech-to-text/realtime",
                listener.local_addr().unwrap()
            ),
            "en",
            &[],
        );
        let handle = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            #[allow(clippy::result_large_err)]
            let check = |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         response| {
                assert_eq!(request.headers()["xi-api-key"], KEY);
                assert!(!request.uri().to_string().contains(KEY));
                Ok(response)
            };
            let mut socket = tokio_tungstenite::accept_hdr_async(connection, check)
                .await
                .unwrap();
            if start {
                socket
                    .send(event(serde_json::json!({"message_type": "session_started", "session_id": "s", "config": {}})))
                    .await
                    .unwrap();
            }
            script(socket).await;
        });
        (url, handle)
    }

    /// Reads audio frames until the commit, returning the PCM, the per-frame sizes and the
    /// commit frame's own audio length.
    async fn until_commit(socket: &mut Socket) -> (Vec<i16>, Vec<usize>) {
        let (mut samples, mut sizes) = (Vec::new(), Vec::new());
        while let Some(Ok(message)) = socket.next().await {
            let Message::Text(text) = message else {
                continue;
            };
            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(frame["message_type"], "input_audio_chunk");
            assert_eq!(frame["sample_rate"], 16000);
            let bytes = decode(frame["audio_base_64"].as_str().unwrap());
            sizes.push(bytes.len());
            samples.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b)),
            );
            if frame["commit"] == true {
                return (samples, sizes);
            }
        }
        panic!("missing commit");
    }

    fn decode(text: &str) -> Vec<u8> {
        let value = |c: u8| {
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
                .iter()
                .position(|&a| a == c)
                .unwrap() as u32
        };
        let mut out = Vec::new();
        for group in text.as_bytes().chunks(4) {
            let n = group.iter().enumerate().fold(0, |n, (i, &c)| {
                n | if c == b'=' {
                    0
                } else {
                    value(c) << (18 - 6 * i)
                }
            });
            let len = group.iter().filter(|&&c| c != b'=').count() - 1;
            out.extend(&n.to_be_bytes()[1..1 + len]);
        }
        out
    }

    #[test]
    fn base64_matches_the_standard_encoding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&base64(&bytes)), bytes);
    }

    #[test]
    fn the_url_is_raw_manual_pcm16_with_filtered_keyterms() {
        let url = scribe_url(SCRIBE_URL, "en", &[]);
        for pair in [
            "model_id=scribe_v2_realtime",
            "audio_format=pcm_16000",
            "commit_strategy=manual",
            "no_verbatim=false",
            "language_code=en",
        ] {
            assert!(url.contains(pair), "{pair}");
        }
        assert!(!url.contains("keyterms"), "prose windows send none");
        assert!(!scribe_url(SCRIBE_URL, "auto", &[]).contains("language_code"));
        let terms: Vec<String> = [
            "9Router",
            "whisper-large-v3-turbo",
            "T3 Code",
            "9router",
            "src-tauri/src/main.rs",
        ]
        .map(String::from)
        .to_vec();
        let parsed = reqwest::Url::parse(&scribe_url(SCRIBE_URL, "en", &terms)).unwrap();
        let sent: Vec<_> = parsed
            .query_pairs()
            .filter(|(k, _)| k == "keyterms")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(sent, ["9Router", "T3 Code"]);
        let many: Vec<String> = (0..80).map(|i| format!("term{i}")).collect();
        assert_eq!(
            scribe_url(SCRIBE_URL, "en", &many)
                .matches("keyterms=")
                .count(),
            SCRIBE_MAX_KEYTERMS
        );
    }

    #[tokio::test]
    async fn early_audio_is_replayed_in_order_and_only_the_committed_text_is_kept() {
        let (url, server) = server(true, |mut socket| async move {
            let (samples, sizes) = until_commit(&mut socket).await;
            let expected: Vec<i16> = (0..30).flat_map(chunk).collect();
            assert_eq!(samples, expected);
            // 50 ms capture chunks go out as they arrive; the commit frame then carries none.
            assert_eq!(sizes.last(), Some(&0));
            assert!(sizes[..sizes.len() - 1].iter().all(|&n| n == 1600));
            socket.send(partial("deploy tomorrow")).await.unwrap();
            socket.send(partial("Deploy tomorrow, no, change")).await.unwrap();
            socket
                .send(committed("Deploy tomorrow, no, change that to Monday."))
                .await
                .unwrap();
            // A late edit or timestamps event must not change or delay the result.
            socket
                .send(event(serde_json::json!({"message_type": "edited_transcript", "text": "x", "edited_text": "y"})))
                .await
                .ok();
        })
        .await;
        let (session, sink) = session(url, 50);
        for i in 0..30 {
            sink.push(&chunk(i));
        }
        assert!(session.stats.lock().unwrap().ready.is_none());
        drop(sink);
        let stats = session.stats.clone();
        assert_eq!(
            session.finish(Duration::from_secs(3)).await.unwrap(),
            "Deploy tomorrow, no, change that to Monday."
        );
        let stats = *stats.lock().unwrap();
        assert!(stats.ready.unwrap() <= stats.first_sent.unwrap());
        assert!(stats.finalize_sent.unwrap() <= stats.first_final.unwrap());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn larger_frames_aggregate_capture_chunks_without_losing_the_tail() {
        let (url, server) = server(true, |mut socket| async move {
            let (samples, sizes) = until_commit(&mut socket).await;
            assert_eq!(samples.len(), 5 * 800);
            assert_eq!(sizes, [3200, 3200, 1600]);
            socket.send(committed("ok")).await.unwrap();
        })
        .await;
        let (session, sink) = session(url, 100);
        for i in 0..5 {
            sink.push(&chunk(i));
        }
        drop(sink);
        assert_eq!(session.finish(Duration::from_secs(3)).await.unwrap(), "ok");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_commit_before_release_is_kept_and_does_not_end_the_session() {
        // The server commits on its own after ~36 s of audio.
        let (url, server) = server(true, |mut socket| async move {
            socket.next().await;
            socket.send(committed("First segment.")).await.unwrap();
            until_commit(&mut socket).await;
            socket.send(committed("Second segment.")).await.unwrap();
        })
        .await;
        let (session, sink) = session(url, 50);
        sink.push(&chunk(1));
        tokio::time::sleep(Duration::from_millis(400)).await;
        sink.push(&chunk(2));
        drop(sink);
        assert_eq!(
            session.finish(Duration::from_secs(3)).await.unwrap(),
            "First segment. Second segment."
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn ending_without_a_committed_transcript_fails_without_partial_text() {
        for close in [true, false] {
            let (url, server) = server(true, move |mut socket| async move {
                until_commit(&mut socket).await;
                socket.send(partial("partial words")).await.unwrap();
                if close {
                    socket.close(None).await.unwrap();
                }
            })
            .await;
            let (session, sink) = session(url, 50);
            sink.push(&chunk(1));
            drop(sink);
            let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
            assert_eq!(reason(&error), "disconnected");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_silent_server_hits_the_finalize_deadline() {
        let (url, server) = server(true, |mut socket| async move {
            until_commit(&mut socket).await;
            socket.send(partial("partial")).await.unwrap();
            while socket.next().await.is_some() {}
        })
        .await;
        let (session, sink) = session(url, 50);
        sink.push(&chunk(1));
        drop(sink);
        let error = session
            .finish(Duration::from_millis(500))
            .await
            .unwrap_err();
        assert_eq!(reason(&error), "finalize_timeout");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn provider_errors_are_classified_without_the_key_or_server_text() {
        let cases = [
            ("auth_error", "auth"),
            ("rate_limited", "rate_limited"),
            ("quota_exceeded", "rate_limited"),
            ("commit_throttled", "rate_limited"),
            ("invalid_request", "provider_error"),
            ("transcriber_error", "provider_error"),
        ];
        for (message_type, expected) in cases {
            let (url, server) = server(false, move |mut socket| async move {
                socket
                    .send(event(serde_json::json!({"message_type": message_type, "error": format!("bad key {KEY}")})))
                    .await
                    .unwrap();
            })
            .await;
            let (session, sink) = session(url, 50);
            sink.push(&chunk(1));
            drop(sink);
            let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
            assert_eq!(reason(&error), expected, "{message_type}");
            assert!(!format!("{error:?}").contains(KEY));
            assert!(!format!("{error:?}").contains("bad key"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn an_error_after_session_start_fails_the_session() {
        let (url, server) = server(true, |mut socket| async move {
            socket.next().await;
            socket
                .send(event(
                    serde_json::json!({"message_type": "error", "error": "boom"}),
                ))
                .await
                .unwrap();
        })
        .await;
        let (session, sink) = session(url, 50);
        sink.push(&chunk(1));
        let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
        assert_eq!(reason(&error), "provider_error");
        drop(sink);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_handshakes_are_classified() {
        for (status, expected) in [
            ("401 Unauthorized", "auth"),
            ("429 Too Many Requests", "rate_limited"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let (mut connection, _) = listener.accept().await.unwrap();
                connection
                    .write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
            });
            let (session, sink) = session(format!("ws://{address}/"), 50);
            drop(sink);
            let error = session.finish(Duration::from_secs(3)).await.unwrap_err();
            assert_eq!(reason(&error), expected);
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelling_closes_the_socket_without_committing() {
        let (heard_sender, heard) = tokio::sync::oneshot::channel();
        let (url, server) = server(true, |mut socket| async move {
            assert!(matches!(socket.next().await, Some(Ok(Message::Text(_)))));
            heard_sender.send(()).unwrap();
            loop {
                match socket.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(Message::Text(text))) => assert!(!text.contains("\"commit\":true")),
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        let (session, sink) = session(url, 50);
        sink.push(&chunk(1));
        heard.await.unwrap();
        drop(session);
        sink.push(&chunk(2));
        assert!(sink.stopped.load(Ordering::SeqCst));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}
