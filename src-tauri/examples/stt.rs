//! Muse regression driver: streams fixed WAV clips through the app's own Muse session at real
//! time and prints one JSON line per clip. No cleanup, no app, no audio device. Driven by
//! `bench/run.py stt`; reads jobs from stdin:
//! `[{"id": "...", "audio": "/abs/clip.wav", "keyterms": ["9Router"]}]`.
//! `cargo run --example stt -- [concurrency] < jobs.json`
use futures_util::StreamExt;
use std::time::{Duration, Instant};
use voice_prompt::settings;
use voice_prompt::streaming;

#[derive(serde::Deserialize)]
struct Job {
    id: String,
    audio: String,
    #[serde(default)]
    keyterms: Vec<String>,
}

/// The capture thread's fragment: 800 samples (50 ms) at 16 kHz.
const CHUNK: usize = 800;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let settings = settings::load(&settings::config_dir());
    let concurrency = std::env::args().nth(1).map_or(Ok(2), |n| n.parse())?;
    let jobs: Vec<Job> = serde_json::from_reader(std::io::stdin())?;
    futures_util::stream::iter(jobs)
        .map(|job| run(&settings, job))
        .buffer_unordered(concurrency)
        .for_each(|line| async move { println!("{line}") })
        .await;
    Ok(())
}

async fn run(settings: &settings::Settings, job: Job) -> String {
    let samples = match std::fs::read(&job.audio)
        .map_err(anyhow::Error::from)
        .and_then(|b| wav_samples(&b))
    {
        Ok(samples) => samples,
        Err(error) => {
            return serde_json::json!({"id": job.id, "error": format!("{error:#}")}).to_string()
        }
    };
    // Press: the session starts connecting while audio queues, as in the app.
    let pressed = Instant::now();
    let (session, sink) = streaming::start(settings, &job.keyterms);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    for chunk in samples.chunks(CHUNK) {
        tick.tick().await;
        sink.push(chunk);
    }
    // Release: dropping the sink ends the audio and finalizes.
    drop(sink);
    let released = Instant::now();
    let conclusion = session.conclude().await;
    let finished = Instant::now();
    let since = |at: Option<Instant>, from: Instant| {
        at.map(|at| at.saturating_duration_since(from).as_millis() as u64)
    };
    let stats = conclusion.stats;
    let (text, error) = match conclusion.transcript {
        Ok(streamed) => (Some(streamed.text), None),
        Err(reason) => (None, Some(reason)),
    };
    serde_json::json!({
        "id": job.id,
        "text": text,
        "error": error,
        "audio_ms": samples.len() as u64 * 1000 / 16_000,
        "ready_ms": since(stats.ready, pressed),
        "connected_ms": since(stats.connected, pressed),
        "first_partial_ms": stats.first_sent.and_then(|sent| since(stats.first_partial, sent)),
        "release_to_final_ms": (finished - released).as_millis() as u64,
    })
    .to_string()
}

/// PCM16 samples from a 16 kHz mono WAV.
fn wav_samples(bytes: &[u8]) -> anyhow::Result<Vec<i16>> {
    let data = bytes
        .windows(4)
        .position(|w| w == b"data")
        .ok_or_else(|| anyhow::anyhow!("no data chunk"))?;
    let (pairs, _) = bytes[data + 8..].as_chunks::<2>();
    Ok(pairs.iter().map(|b| i16::from_le_bytes(*b)).collect())
}
