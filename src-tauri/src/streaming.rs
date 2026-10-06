use crate::{audio, providers::Transcript, settings::Settings};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::Request, Message};

const QUEUE_BLOCKS: usize = 100;

pub struct Input {
    sender: mpsc::Sender<Vec<u8>>,
    failed: Arc<AtomicBool>,
    min_hold: Duration,
    speech: bool,
    checked_samples: usize,
    sent_samples: usize,
}

impl Input {
    pub fn push(&mut self, samples: &[i16], held: Duration) {
        if self.failed.load(Ordering::SeqCst) {
            return;
        }
        self.speech |= !audio::is_silent(&samples[self.checked_samples..]);
        self.checked_samples = samples.len();
        if held < self.min_hold || !self.speech {
            return;
        }
        let bytes = samples[self.sent_samples..]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        if self.sender.try_send(bytes).is_err() {
            self.failed.store(true, Ordering::SeqCst);
        } else {
            self.sent_samples = samples.len();
        }
    }
}

pub struct Session {
    task: tauri::async_runtime::JoinHandle<Result<Transcript>>,
    timeout: Duration,
    failed: Arc<AtomicBool>,
}

impl Session {
    pub fn start(settings: &Settings) -> (Self, Input) {
        let (sender, receiver) = mpsc::channel(QUEUE_BLOCKS);
        let failed = Arc::new(AtomicBool::new(false));
        let settings = settings.clone();
        let timeout = Duration::from_secs(settings.stt_timeout_secs);
        let min_hold = Duration::from_millis(settings.min_hold_ms);
        let task_failed = failed.clone();
        let task = tauri::async_runtime::spawn(async move {
            let key = tauri::async_runtime::spawn_blocking(|| {
                crate::settings::secret::get_for("deepgram")
            })
            .await??
            .ok_or_else(|| anyhow!("No Deepgram API key. Open Settings → Speech."))?;
            let request = request(&settings, &key)?;
            transcribe(request, receiver, task_failed, timeout).await
        });
        (
            Self {
                task,
                timeout,
                failed: failed.clone(),
            },
            Input {
                sender,
                failed,
                min_hold,
                speech: false,
                checked_samples: 0,
                sent_samples: 0,
            },
        )
    }

    pub async fn finish(mut self) -> Result<Transcript> {
        tokio::time::timeout(self.timeout, &mut self.task)
            .await
            .map_err(|_| anyhow!("Deepgram finalization timed out. The recording can be retried."))?
            .context("Deepgram session stopped")?
    }

    pub async fn replay(settings: &Settings, samples: &[i16]) -> Result<Transcript> {
        let (session, mut input) = Self::start(settings);
        input.push(samples, Duration::from_millis(settings.min_hold_ms));
        drop(input);
        session.finish().await
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.failed.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

fn request(settings: &Settings, key: &str) -> Result<Request<()>> {
    let mut url = reqwest::Url::parse("wss://api.deepgram.com/v1/listen")?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("model", &settings.deepgram_model)
            .append_pair("encoding", "linear16")
            .append_pair("sample_rate", &audio::RATE.to_string())
            .append_pair("channels", "1")
            .append_pair("interim_results", "false")
            .append_pair("smart_format", "true");
        if !settings.language.is_empty() && settings.language != "auto" {
            query.append_pair("language", &settings.language);
        } else {
            query.append_pair("language", "multi");
        }
        let hint = if settings.deepgram_model.starts_with("nova-3") {
            "keyterm"
        } else {
            "keywords"
        };
        for term in &settings.vocabulary {
            let term = term.trim();
            if !term.is_empty() {
                query.append_pair(hint, term);
            }
        }
    }
    let mut request = url.as_str().into_client_request()?;
    let mut authorization = format!("Token {key}")
        .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
        .map_err(|_| anyhow!("Invalid Deepgram API key"))?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("Authorization", authorization);
    Ok(request)
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Event {
    Results {
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

async fn transcribe(
    request: Request<()>,
    mut receiver: mpsc::Receiver<Vec<u8>>,
    failed: Arc<AtomicBool>,
    timeout: Duration,
) -> Result<Transcript> {
    let (socket, _) = tokio::time::timeout(
        timeout.min(Duration::from_secs(5)),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| anyhow!("Deepgram connection timed out"))?
    .map_err(|error| match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            anyhow!("Deepgram rejected the connection ({}). Check the key and model in Settings → Speech.", response.status())
        }
        _ => anyhow!("Cannot connect to Deepgram: {error}"),
    })?;
    let (mut writer, mut reader) = socket.split();
    let closing = AtomicBool::new(false);
    let send = async {
        let mut keep_alive = tokio::time::interval(Duration::from_secs(4));
        loop {
            let message = tokio::select! {
                block = receiver.recv() => match block {
                    Some(bytes) => Message::Binary(bytes.into()),
                    None => {
                        if failed.load(Ordering::SeqCst) {
                            bail!("Deepgram audio delivery failed. The recording can be retried.");
                        }
                        closing.store(true, Ordering::SeqCst);
                        tokio::time::timeout(timeout, writer.send(Message::text(r#"{"type":"CloseStream"}"#)))
                            .await
                            .map_err(|_| anyhow!("Deepgram audio flush timed out"))??;
                        return Ok::<(), anyhow::Error>(());
                    }
                },
                _ = keep_alive.tick() => Message::text(r#"{"type":"KeepAlive"}"#),
            };
            if failed.load(Ordering::SeqCst) {
                bail!("Deepgram audio delivery failed. The recording can be retried.");
            }
            tokio::time::timeout(timeout, writer.send(message))
                .await
                .map_err(|_| anyhow!("Deepgram audio upload timed out"))??;
        }
    };
    let receive = async {
        let mut segments = Vec::new();
        while let Some(message) = reader.next().await {
            match message? {
                Message::Text(text) => {
                    let event: Event = serde_json::from_str(&text)
                        .map_err(|_| anyhow!("Invalid Deepgram transcription response"))?;
                    match event {
                        Event::Results {
                            is_final: true,
                            channel,
                        } => {
                            let alternative =
                                channel.alternatives.into_iter().next().ok_or_else(|| {
                                    anyhow!("Deepgram returned no transcript alternative")
                                })?;
                            let text = alternative.transcript.trim().to_string();
                            if !text.is_empty() {
                                segments.push(text);
                            }
                        }
                        Event::Metadata {} if closing.load(Ordering::SeqCst) => {
                            return Ok(Transcript {
                                text: segments.join(" "),
                            });
                        }
                        Event::Error {} => bail!("Deepgram reported a streaming error"),
                        _ => {}
                    }
                }
                Message::Close(_) => bail!("Deepgram disconnected before transcription completed"),
                Message::Binary(_) => bail!("Unexpected binary response from Deepgram"),
                _ => {}
            }
        }
        bail!("Deepgram disconnected without completing transcription")
    };
    let (_, transcript) = tokio::try_join!(send, receive)?;
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn input_channel(capacity: usize) -> (Input, mpsc::Receiver<Vec<u8>>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Input {
                sender,
                failed: Arc::new(AtomicBool::new(false)),
                min_hold: Duration::from_millis(250),
                speech: false,
                checked_samples: 0,
                sent_samples: 0,
            },
            receiver,
        )
    }

    async fn endpoint() -> (TcpListener, Request<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let request = format!("ws://{}/", listener.local_addr().unwrap())
            .into_client_request()
            .unwrap();
        (listener, request)
    }

    fn result(text: &str, is_final: bool) -> Message {
        Message::text(
            serde_json::json!({
                "type": "Results",
                "is_final": is_final,
                "channel": {"alternatives": [{"transcript": text}]},
            })
            .to_string(),
        )
    }

    #[test]
    fn speech_gate_retains_prefix_and_sends_each_sample_once() {
        let (mut input, mut receiver) = input_channel(2);
        let mut samples = vec![0; 800];
        input.push(&samples, Duration::from_millis(50));
        samples.extend(vec![2000; 800]);
        input.push(&samples, Duration::from_millis(100));
        assert!(receiver.try_recv().is_err());
        samples.extend(vec![0; 2400]);
        input.push(&samples, Duration::from_millis(250));
        let prefix = receiver.try_recv().unwrap();
        assert_eq!(prefix.len(), samples.len() * 2);
        assert_eq!(&prefix[1600..1602], &2000i16.to_le_bytes());
        samples.extend(vec![3000; 800]);
        input.push(&samples, Duration::from_millis(300));
        let tail = receiver.try_recv().unwrap();
        assert_eq!(tail.len(), 1600);
        assert_eq!(&tail[..2], &3000i16.to_le_bytes());
    }

    #[test]
    fn silence_is_not_transmitted() {
        let (mut input, mut receiver) = input_channel(1);
        input.push(&vec![3; 16_000], Duration::from_secs(1));
        assert!(receiver.try_recv().is_err());
        assert!(!input.failed.load(Ordering::Relaxed));
    }

    #[test]
    fn a_full_queue_marks_the_session_failed_without_blocking_capture() {
        let (mut input, mut receiver) = input_channel(1);
        let mut samples = vec![2000; 800];
        input.push(&samples, Duration::from_millis(250));
        samples.extend(vec![3000; 800]);
        input.push(&samples, Duration::from_millis(300));
        assert!(input.failed.load(Ordering::Relaxed));
        assert_eq!(samples.len(), 1600);
        assert_eq!(receiver.try_recv().unwrap().len(), 1600);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn query_encodes_vocabulary_and_redacts_credentials() {
        let settings = Settings {
            vocabulary: vec!["T3 Code & 9Router".into()],
            ..Settings::default()
        };
        let request = request(&settings, "test-key").unwrap();
        let url = reqwest::Url::parse(&request.uri().to_string()).unwrap();
        assert!(url
            .query_pairs()
            .any(|(name, value)| { name == "keyterm" && value == "T3 Code & 9Router" }));
        assert!(url
            .query_pairs()
            .any(|(name, value)| name == "sample_rate" && value == "16000"));
        assert!(request.headers()["Authorization"].is_sensitive());
        assert!(super::request(&settings, "invalid\nkey").is_err());
    }

    #[tokio::test]
    async fn streams_before_release_and_waits_for_the_final_tail() {
        let (listener, request) = endpoint().await;
        let (mut input, receiver) = input_channel(2);
        let failed = input.failed.clone();
        let (heard_sender, heard_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(connection).await.unwrap();
            let mut heard_sender = Some(heard_sender);
            let mut audio = Vec::new();
            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    Message::Binary(bytes) => {
                        audio.extend_from_slice(&bytes);
                        if let Some(sender) = heard_sender.take() {
                            socket.send(result("Wrong interim", false)).await.unwrap();
                            socket.send(result("Use T3 Code", true)).await.unwrap();
                            sender.send(()).unwrap();
                        }
                    }
                    Message::Text(text) if text.contains("CloseStream") => {
                        assert_eq!(audio.len(), 3200);
                        assert_eq!(&audio[..2], &2000i16.to_le_bytes());
                        assert_eq!(&audio[1600..1602], &3000i16.to_le_bytes());
                        socket
                            .send(result("with config.toml.", true))
                            .await
                            .unwrap();
                        socket
                            .send(Message::text(r#"{"type":"Metadata"}"#))
                            .await
                            .unwrap();
                        return;
                    }
                    _ => {}
                }
            }
            panic!("Missing CloseStream");
        });
        let task = tokio::spawn(transcribe(
            request,
            receiver,
            failed,
            Duration::from_secs(2),
        ));
        let mut samples = vec![2000; 800];
        input.push(&samples, Duration::from_millis(250));
        tokio::time::timeout(Duration::from_secs(2), heard_receiver)
            .await
            .unwrap()
            .unwrap();
        assert!(!task.is_finished());
        samples.extend(vec![3000; 800]);
        input.push(&samples, Duration::from_millis(300));
        drop(input);
        let transcript = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(transcript.text, "Use T3 Code with config.toml.");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn early_disconnect_never_returns_a_partial_transcript() {
        let (listener, request) = endpoint().await;
        let (input, receiver) = input_channel(1);
        let failed = input.failed.clone();
        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(connection).await.unwrap();
            socket.send(result("Partial prompt", true)).await.unwrap();
            socket.close(None).await.unwrap();
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            transcribe(request, receiver, failed, Duration::from_secs(1)),
        )
        .await
        .unwrap();
        assert!(outcome
            .err()
            .unwrap()
            .to_string()
            .contains("before transcription completed"));
        drop(input);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn provider_errors_never_return_a_partial_transcript() {
        let (listener, request) = endpoint().await;
        let (input, receiver) = input_channel(1);
        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(connection).await.unwrap();
            socket.send(result("Partial prompt", true)).await.unwrap();
            socket
                .send(Message::text(r#"{"type":"Error","description":"failure"}"#))
                .await
                .unwrap();
        });
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            transcribe(
                request,
                receiver,
                input.failed.clone(),
                Duration::from_secs(1),
            ),
        )
        .await
        .unwrap();
        assert!(outcome
            .err()
            .unwrap()
            .to_string()
            .contains("streaming error"));
        drop(input);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn finalization_timeout_aborts_the_socket() {
        let (listener, request) = endpoint().await;
        let (input, receiver) = input_channel(1);
        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(connection).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if let Message::Text(text) = message {
                    if text.contains("CloseStream") {
                        assert!(matches!(
                            socket.next().await,
                            None | Some(Err(_)) | Some(Ok(Message::Close(_)))
                        ));
                        return;
                    }
                }
            }
            panic!("Missing CloseStream");
        });
        let task = tokio::spawn(transcribe(
            request,
            receiver,
            input.failed.clone(),
            Duration::from_secs(1),
        ));
        let session = Session {
            task: tauri::async_runtime::JoinHandle::Tokio(task),
            timeout: Duration::from_millis(500),
            failed: input.failed.clone(),
        };
        drop(input);
        let error = session.finish().await.err().unwrap();
        assert!(error.to_string().contains("finalization timed out"));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancelling_drops_the_socket_even_while_capture_is_active() {
        let (listener, request) = endpoint().await;
        let (mut input, receiver) = input_channel(1);
        let (heard_sender, heard_receiver) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (connection, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(connection).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Binary(_)) {
                    heard_sender.send(()).unwrap();
                    loop {
                        match socket.next().await {
                            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                            Some(Ok(Message::Text(text))) => {
                                assert!(text.contains("KeepAlive"));
                            }
                            _ => panic!("Unexpected audio after cancellation"),
                        }
                    }
                    return;
                }
            }
            panic!("Missing audio");
        });
        let task = tokio::spawn(transcribe(
            request,
            receiver,
            input.failed.clone(),
            Duration::from_secs(1),
        ));
        let session = Session {
            task: tauri::async_runtime::JoinHandle::Tokio(task),
            timeout: Duration::from_secs(1),
            failed: input.failed.clone(),
        };
        input.push(&vec![2000; 800], Duration::from_millis(250));
        tokio::time::timeout(Duration::from_secs(2), heard_receiver)
            .await
            .unwrap()
            .unwrap();
        drop(session);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        drop(input);
    }

    #[tokio::test]
    async fn a_short_tap_cannot_send_a_late_capture_callback() {
        let (mut input, mut receiver) = input_channel(1);
        let samples = vec![2000; 800];
        input.push(&samples, Duration::from_millis(240));
        let task = tokio::spawn(std::future::pending::<Result<Transcript>>());
        let session = Session {
            task: tauri::async_runtime::JoinHandle::Tokio(task),
            timeout: Duration::from_secs(1),
            failed: input.failed.clone(),
        };
        drop(session);
        input.push(&samples, Duration::from_millis(260));
        assert!(receiver.try_recv().is_err());
        assert_eq!(input.sent_samples, 0);
    }

    #[tokio::test]
    async fn a_close_without_terminal_metadata_never_completes_finalization() {
        use tokio_tungstenite::tungstenite::{
            protocol::frame::coding::CloseCode, protocol::CloseFrame,
        };
        for frame in [
            None,
            Some(CloseFrame {
                code: CloseCode::Away,
                reason: "shutting down".into(),
            }),
            Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }),
        ] {
            let (listener, request) = endpoint().await;
            let (mut input, receiver) = input_channel(1);
            let server = tokio::spawn(async move {
                let (connection, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(connection).await.unwrap();
                while let Some(Ok(message)) = socket.next().await {
                    match message {
                        Message::Binary(_) => {
                            socket.send(result("Partial prompt", true)).await.unwrap();
                        }
                        Message::Text(text) if text.contains("CloseStream") => {
                            socket.send(Message::Close(frame)).await.unwrap();
                            return;
                        }
                        _ => {}
                    }
                }
                panic!("Missing CloseStream");
            });
            let failed = input.failed.clone();
            input.push(&vec![2000; 800], Duration::from_millis(250));
            drop(input);
            let outcome = tokio::time::timeout(
                Duration::from_secs(2),
                transcribe(request, receiver, failed, Duration::from_secs(1)),
            )
            .await
            .unwrap();
            assert!(outcome
                .err()
                .unwrap()
                .to_string()
                .contains("before transcription completed"));
            server.await.unwrap();
        }
    }
}
