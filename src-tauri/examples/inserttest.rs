//! Inserts stdin into the focused window and prints the timing as JSON:
//! `echo hi | cargo run --example inserttest -- type|paste|paste-terminal|auto [--revalidate MS]`
//! `--revalidate MS` snapshots the active window, waits MS, then delivers like the app: insert
//! only if the same window is still active, otherwise copy.
#[path = "../src/insertion.rs"]
#[allow(dead_code)]
mod insertion;
#[path = "../src/target.rs"]
#[allow(dead_code)]
mod target;
use std::io::Read;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let configured = insertion::Method::parse(args.get(1).map_or("", String::as_str));
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let tracker = target::Tracker::start().ok();
    let pressed = tracker.as_ref().and_then(target::Tracker::active);
    if let Some(wait) = args
        .iter()
        .position(|a| a == "--revalidate")
        .and_then(|i| args.get(i + 1))
    {
        std::thread::sleep(std::time::Duration::from_millis(wait.parse()?));
    }
    let started = std::time::Instant::now();
    let now = tracker.as_ref().map(target::Tracker::active);
    let destination = match &now {
        Some(now) => target::revalidate(pressed.as_ref(), now.as_ref()),
        None => target::Destination::Unknown,
    };
    let window = match &destination {
        target::Destination::Confirmed(window) => Some(window.clone()),
        _ => None,
    };
    let method = match destination {
        target::Destination::Changed { .. } => insertion::Method::Clipboard,
        target::Destination::Unknown if now.is_some() => insertion::Method::Clipboard,
        _ => insertion::choose(configured, window.as_ref(), &text),
    };
    let timing = insertion::insert(method, &text)?;
    println!(
        "{{\"method\":\"{}\",\"destination\":\"{}\",\"app\":\"{}\",\"setup_ms\":{},\"keys_ms\":{},\"clipboard_ms\":{},\"total_ms\":{}}}",
        method.name(),
        destination.label(),
        window.map_or(String::new(), |w| w.app_id),
        timing.setup_ms,
        timing.keys_ms,
        timing.clipboard_ms,
        started.elapsed().as_millis()
    );
    // The clipboard is served from this process (as in the app); stay alive so the target can read it.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    Ok(())
}
