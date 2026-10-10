//! Manual end-to-end check through the app's pipeline, with the app's settings and keyring key:
//! `cargo run --example pipeline -- audio.wav [--type]`. Without `--type` the result is copied.
//! `VP_CLEANUP_MODEL` overrides the cleanup model. Uses a throwaway history.
use std::sync::Arc;
use std::time::{Duration, Instant};
use voice_prompt::pipeline::{Pipeline, Recording, Snapshot};
use voice_prompt::{delivery, history::HistoryStore, providers, settings};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let samples = wav_samples(&std::fs::read(&args[1])?)?;
    let mut settings = settings::load(&settings::config_dir());
    if let Ok(model) = std::env::var("VP_CLEANUP_MODEL") {
        settings.cleanup_model = model;
    }
    settings.auto_insert = args.iter().any(|a| a == "--type");
    let dir = tempfile::tempdir()?;
    let client = reqwest::Client::new();
    let connect = Box::new(move |settings: &settings::Settings| {
        let key = settings::secret::get()?.ok_or_else(|| anyhow::anyhow!("no API key"))?;
        Ok(providers::connect(client.clone(), settings, key))
    });
    let desktop = delivery::System {
        windows: None,
        keys_held: Box::new(|| false),
    };
    let pipeline = Pipeline::new(
        HistoryStore::open(&dir.path().join("history.db"))?,
        connect,
        Arc::new(desktop),
    );
    if settings.auto_insert {
        std::thread::sleep(Duration::from_secs(2));
    }
    let started = Instant::now();
    let recording = Recording::retry(samples);
    let snapshot = Snapshot::take(settings, None);
    let outcome = pipeline
        .run(&snapshot, recording, |stage| {
            println!("{stage} at {} ms", started.elapsed().as_millis())
        })
        .await?;
    let entry = pipeline.history.lock().unwrap().list(1)?.pop();
    println!("{} ms: {outcome:?}", started.elapsed().as_millis());
    if let Some(entry) = entry {
        println!("STT ({}): {}", entry.stt_model, entry.raw);
        println!("Cleaned: {}", entry.cleaned.unwrap_or_default());
    }
    // The clipboard is served from this process; stay alive so it can be pasted.
    std::thread::sleep(Duration::from_millis(1500));
    Ok(())
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
