//! Live transcription: PCM streams from the capture thread straight to the provider while
//! the key is held. Audio never passes through 9Router.
//!
//! - Deepgram (default): 9Router mints a short-lived token. After `CloseStream`, Deepgram
//!   flushes the remaining results, sends `Metadata`, then closes; only that `Metadata`
//!   completes a session. `is_final`, `speech_final`, silence or a bare close never do.
//! - Meta Muse (experimental): the Meta key goes in the first JSON frame, not a header.
//!   After `endStream`, a push-to-talk session ends with one `transcript` event marked
//!   `final: true` and then a normal close (1000); anything else falls back to batch STT.
use crate::{audio, settings::Settings};
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

pub fn reason(error: &anyhow::Error) -> &'static str {
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
    /// `deepgram` | `muse`
    pub provider: &'static str,
    task: tauri::async_runtime::JoinHandle<Result<String>>,
    stopped: Arc<AtomicBool>,
    overflowed: Arc<AtomicBool>,
    pub stats: Arc<Mutex<Stats>>,
}

impl StreamSession {
    /// The complete transcript, once the provider confirms the session finished. Call after the
    /// sink is dropped; fails at `deadline` rather than returning partial text.
    pub async fn finish(mut self, deadline: Duration) -> Result<String> {
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

/// Starts a session whose audio is queued until the provider connection is ready.
pub trait StreamingTranscriber {
    fn start(&self, keyterms: &[String]) -> (StreamSession, AudioSink);
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
}

impl StreamingTranscriber for DeepgramStreamingTranscriber {
    fn start(&self, keyterms: &[String]) -> (StreamSession, AudioSink) {
        let url = listen_url(LISTEN_URL, &self.model, &self.language, keyterms);
        let token = Box::pin(token(self.client.clone(), self.endpoint.clone()));
        spawn("deepgram", move |audio| deepgram(token, url, audio))
    }
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
}

impl StreamingTranscriber for MuseStreamingTranscriber {
    fn start(&self, keyterms: &[String]) -> (StreamSession, AudioSink) {
        let language = self.language_bias.then_some(self.language.as_str());
        let config = muse_config(language, keyterms);
        let key: TokenFuture = Box::pin(async {
            tauri::async_runtime::spawn_blocking(|| {
                crate::settings::secret::get_for(crate::settings::secret::MUSE)
            })
            .await
            .map_err(|_| fail("auth", "Keyring read stopped"))??
            .ok_or_else(|| fail("auth", "No Meta Muse API key. Open Settings → Speech."))
        });
        spawn("muse", move |audio| {
            muse(key, MUSE_URL.into(), config, audio)
        })
    }
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
fn spawn<F, Fut>(provider: &'static str, run: F) -> (StreamSession, AudioSink)
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
            provider,
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
        spawn("deepgram", move |audio| deepgram(token, url, audio))
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
        spawn("muse", move |audio| muse(key, url, config, audio))
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
        let terms: Vec<String> = ["9Router", "T3 Code", "pnpm", "9router"]
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
