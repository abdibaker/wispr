//! Inserts stdin into the focused window and prints the timing as JSON:
//! `echo hi | cargo run --example inserttest -- type|paste|paste-terminal`
#[path = "../src/insertion.rs"]
#[allow(dead_code)]
mod insertion;
use std::io::Read;

fn main() -> anyhow::Result<()> {
    let method = insertion::Method::parse(&std::env::args().nth(1).unwrap_or_default());
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let started = std::time::Instant::now();
    let timing = insertion::insert(method, &text)?;
    println!(
        "{{\"setup_ms\":{},\"keys_ms\":{},\"clipboard_ms\":{},\"total_ms\":{}}}",
        timing.setup_ms,
        timing.keys_ms,
        timing.clipboard_ms,
        started.elapsed().as_millis()
    );
    // The clipboard is served from this process (as in the app); stay alive so the target can read it.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    Ok(())
}
