//! Manual end-to-end check: `VP_KEY=... [VP_CLEANUP_MODEL=...] cargo run --example pipeline -- audio.wav [--type]`
#[path = "../src/insertion.rs"]
#[allow(dead_code)]
mod insertion;
#[path = "../src/providers.rs"]
#[allow(dead_code)]
mod providers;
#[path = "../src/settings.rs"]
#[allow(dead_code)]
mod settings;
#[path = "../src/target.rs"]
#[allow(dead_code)]
mod target;

use providers::*;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let wav = std::fs::read(&args[1])?;
    let defaults = settings::Settings::default();
    let provider = OpenAiCompatible {
        client: reqwest::Client::new(),
        base_url: std::env::var("VP_ENDPOINT").unwrap_or(defaults.endpoint.clone()),
        api_key: std::env::var("VP_KEY")?,
        model: defaults.stt_model.clone(),
        timeout: Duration::from_secs(30),
        reasoning_effort: String::new(),
    };
    let started = Instant::now();
    let transcript = provider
        .transcribe(wav, "en", settings::vocabulary_hint(&defaults.vocabulary))
        .await?;
    println!(
        "STT {} ms: {}",
        started.elapsed().as_millis(),
        transcript.text
    );
    let cleaner = OpenAiCompatible {
        model: std::env::var("VP_CLEANUP_MODEL").unwrap_or(defaults.cleanup_model.clone()),
        reasoning_effort: "low".into(),
        timeout: Duration::from_secs(15),
        ..provider
    };
    let started = Instant::now();
    let cleaned = cleaner
        .clean(&transcript.text, &defaults.vocabulary, "")
        .await?;
    println!(
        "Cleanup {} ms: {}",
        started.elapsed().as_millis(),
        cleaned.text
    );
    if args.iter().any(|a| a == "--type") {
        std::thread::sleep(Duration::from_secs(2));
        insertion::insert(insertion::Method::Type, &cleaned.text)?;
    }
    Ok(())
}
