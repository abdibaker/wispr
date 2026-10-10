//! Inserts stdin into the focused window and prints the timing as JSON:
//! `echo hi | cargo run --example inserttest -- type|paste|paste-terminal|auto [--revalidate MS]`
//! `--revalidate MS` snapshots the active window, waits MS, then delivers like the app: insert
//! only if the same window is still active, otherwise copy.
use std::io::Read;
use std::sync::Arc;
use voice_prompt::{delivery, insertion, settings::Settings, target};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let settings = Settings {
        insertion_method: insertion::Method::parse(args.get(1).map_or("", String::as_str)),
        keep_in_clipboard: false,
        ..Default::default()
    };
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let desktop = delivery::System {
        windows: target::Tracker::start().ok(),
        // No hotkey here: the caller is not holding the shortcut.
        keys_held: Box::new(|| false),
    };
    let pressed = desktop.windows.as_ref().and_then(target::Tracker::active);
    if let Some(wait) = args
        .iter()
        .position(|a| a == "--revalidate")
        .and_then(|i| args.get(i + 1))
    {
        std::thread::sleep(std::time::Duration::from_millis(wait.parse()?));
    }
    let started = std::time::Instant::now();
    let delivered =
        delivery::deliver(Arc::new(desktop), &settings, &text, pressed.as_ref()).await?;
    let method = delivered
        .delivery
        .strip_prefix("copied_")
        .map_or(delivered.delivery.as_str(), |_| "copied");
    println!(
        "{{\"method\":\"{}\",\"delivery\":\"{}\",\"destination\":\"{}\",\"app\":\"{}\",\"setup_ms\":{},\"keys_ms\":{},\"clipboard_ms\":{},\"total_ms\":{}}}",
        method,
        delivered.delivery,
        if delivered.destination.is_empty() { "unknown" } else { delivered.destination },
        pressed.map_or(String::new(), |w| w.app_id),
        delivered.insert.setup_ms,
        delivered.insert.keys_ms,
        delivered.insert.clipboard_ms,
        started.elapsed().as_millis()
    );
    // The clipboard is served from this process (as in the app); stay alive so the target can read it.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    Ok(())
}
