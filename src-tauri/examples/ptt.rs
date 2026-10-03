//! Test helper: holds a chord on a virtual uinput keyboard.
//! `cargo run --example ptt -- 4000` holds Ctrl+Super for 4 s; `setkey` stores $VP_KEY in the keyring.
#[path = "../src/settings.rs"]
#[allow(dead_code)]
mod settings;
use evdev::{uinput::VirtualDevice, AttributeSet, EventType, InputEvent, KeyCode};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "3000".into());
    if arg == "setkey" {
        return settings::secret::set(&std::env::var("VP_KEY")?);
    }
    let mut keys = AttributeSet::<KeyCode>::new();
    for key in [KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTMETA, KeyCode::KEY_A] {
        keys.insert(key);
    }
    let mut device = VirtualDevice::builder()?
        .name("voice-prompt-test")
        .with_keys(&keys)?
        .build()?;
    // Let the app's 3 s hotplug poll (worst case 3 s + open) and the compositor pick the device up.
    std::thread::sleep(Duration::from_millis(7000));
    let key = |code: KeyCode, value| InputEvent::new(EventType::KEY.0, code.0, value);
    device.emit(&[key(KeyCode::KEY_LEFTCTRL, 1)])?;
    std::thread::sleep(Duration::from_millis(30));
    device.emit(&[key(KeyCode::KEY_LEFTMETA, 1)])?;
    println!("pressed");
    std::thread::sleep(Duration::from_millis(arg.parse()?));
    device.emit(&[key(KeyCode::KEY_LEFTMETA, 0)])?;
    device.emit(&[key(KeyCode::KEY_LEFTCTRL, 0)])?;
    println!("released");
    std::thread::sleep(Duration::from_millis(300));
    Ok(())
}
