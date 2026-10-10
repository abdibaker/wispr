//! Types a fixed string into the focused window: `cargo run --example typetest`
use voice_prompt::insertion;

fn main() -> anyhow::Result<()> {
    insertion::insert(
        insertion::Method::Type,
        "Héllo, `src/main.rs` — ünïcode ✓ 9Router\nline two\n",
    )?;
    Ok(())
}
