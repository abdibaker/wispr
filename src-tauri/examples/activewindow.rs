//! Prints the active window as COSMIC reports it: `cargo run --example activewindow [seconds]`.
#[path = "../src/target.rs"]
#[allow(dead_code)]
mod target;

fn main() -> anyhow::Result<()> {
    let tracker = target::Tracker::start()?;
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    for _ in 0..=secs * 2 {
        println!("{:?}", tracker.active());
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}
