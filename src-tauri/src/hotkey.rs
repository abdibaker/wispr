//! HotkeyManager: push-to-talk via evdev key state.
//!
//! COSMIC's xdg-desktop-portal does not implement the GlobalShortcuts portal
//! (pop-os/xdg-desktop-portal-cosmic#4), so we read key state from /dev/input. Read access
//! comes from the shipped uaccess udev rule (or the `input` group fallback). Only key
//! up/down state is kept; key codes are never logged.
//! See docs/research-cosmic-global-shortcuts.md.
use anyhow::{bail, Result};
use evdev::{Device, EventSummary, KeyCode};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum HotkeyEvent {
    Pressed,
    Released,
    /// Another key joined the chord (e.g. Ctrl+Super+Left): the user meant a different shortcut.
    Cancelled,
}

/// A chord is a list of groups; each group is satisfied by any of its keys (Ctrl = left or right).
#[derive(Debug, Clone, PartialEq)]
pub struct Chord(Vec<Vec<KeyCode>>);

pub fn parse(shortcut: &str) -> Result<Chord> {
    let mut groups = Vec::new();
    for part in shortcut.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let keys = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_RIGHTCTRL],
            "super" | "win" | "meta" | "logo" => {
                vec![KeyCode::KEY_LEFTMETA, KeyCode::KEY_RIGHTMETA]
            }
            "alt" => vec![KeyCode::KEY_LEFTALT, KeyCode::KEY_RIGHTALT],
            "shift" => vec![KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_RIGHTSHIFT],
            "altgr" => vec![KeyCode::KEY_RIGHTALT],
            "space" => vec![KeyCode::KEY_SPACE],
            "capslock" => vec![KeyCode::KEY_CAPSLOCK],
            "menu" => vec![KeyCode::KEY_COMPOSE],
            other => match format!("KEY_{}", other.to_ascii_uppercase()).parse::<KeyCode>() {
                Ok(key) => vec![key],
                Err(_) => bail!("Unknown key '{part}'"),
            },
        };
        groups.push(keys);
    }
    if groups.is_empty() {
        bail!("Shortcut is empty");
    }
    Ok(Chord(groups))
}

impl Chord {
    fn held(&self, pressed: &HashSet<KeyCode>) -> bool {
        self.0
            .iter()
            .all(|group| group.iter().any(|k| pressed.contains(k)))
    }
    fn contains(&self, key: KeyCode) -> bool {
        self.0.iter().any(|group| group.contains(&key))
    }
}

/// Tracks chord state from a stream of key events.
pub struct Tracker {
    pressed: HashSet<KeyCode>,
    active: bool,
    cancelled: bool,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            pressed: HashSet::new(),
            active: false,
            cancelled: false,
        }
    }

    pub fn key(&mut self, chord: &Chord, key: KeyCode, down: bool) -> Option<HotkeyEvent> {
        if down {
            self.pressed.insert(key);
        } else {
            self.pressed.remove(&key);
        }
        let held = chord.held(&self.pressed);
        let extra = self.pressed.iter().any(|k| !chord.contains(*k));
        if self.active {
            if extra && down {
                self.active = false;
                self.cancelled = true;
                return Some(HotkeyEvent::Cancelled);
            }
            if !held {
                self.active = false;
                return Some(HotkeyEvent::Released);
            }
        } else if held && !extra && down && !self.cancelled {
            self.active = true;
            return Some(HotkeyEvent::Pressed);
        }
        if self.pressed.is_empty() {
            self.cancelled = false;
        }
        None
    }
}

pub struct HotkeyManager {
    chord: Arc<Mutex<Chord>>,
}

impl HotkeyManager {
    /// Spawns device readers; `on_event` runs on a dedicated thread.
    pub fn start(shortcut: &str, on_event: impl Fn(HotkeyEvent) + Send + 'static) -> Result<Self> {
        let chord = Arc::new(Mutex::new(parse(shortcut)?));
        let (sender, receiver) = channel::<(String, KeyCode, bool)>();
        if keyboards().is_empty() {
            bail!("No readable keyboard in /dev/input. Re-login, or add your user to the 'input' group and log in again.");
        }
        std::thread::spawn(move || watch_devices(sender));
        let tracker_chord = chord.clone();
        std::thread::spawn(move || {
            let mut tracker = Tracker::new();
            // Per-device key state so a device unplugged mid-chord cannot leave keys stuck.
            let mut per_device: HashMap<String, HashSet<KeyCode>> = HashMap::new();
            for (device, key, down) in receiver {
                if key == KeyCode::KEY_RESERVED {
                    for stale in per_device.remove(&device).unwrap_or_default() {
                        let chord = tracker_chord.lock().unwrap();
                        if let Some(event) = tracker.key(&chord, stale, false) {
                            on_event(event);
                        }
                    }
                    continue;
                }
                let keys = per_device.entry(device).or_default();
                if down {
                    keys.insert(key);
                } else {
                    keys.remove(&key);
                }
                let event = {
                    let chord = tracker_chord.lock().unwrap();
                    tracker.key(&chord, key, down)
                };
                if let Some(event) = event {
                    on_event(event);
                }
            }
        });
        Ok(Self { chord })
    }

    /// True while any physical key is down (typing then would combine with held modifiers).
    /// Reads the kernel's live key bitmaps (EVIOCGKEY), so a key held since before startup
    /// counts and a missed release cannot wedge the check.
    pub fn keys_held(&self) -> bool {
        keyboards().into_iter().any(|path| {
            Device::open(path).is_ok_and(|d| {
                d.get_key_state()
                    .is_ok_and(|state| state.iter().next().is_some())
            })
        })
    }

    pub fn set_shortcut(&self, shortcut: &str) -> Result<()> {
        *self.chord.lock().unwrap() = parse(shortcut)?;
        Ok(())
    }
}

fn event_nodes() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("event"))
        })
        .collect()
}

/// Only real keyboards are watched, so mice and touchpads get no reader thread
/// (their BTN_* clicks would otherwise cancel a held chord).
fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|k| k.contains(KeyCode::KEY_LEFTCTRL) && k.contains(KeyCode::KEY_A))
}

fn keyboards() -> Vec<PathBuf> {
    event_nodes()
        .into_iter()
        .filter(|p| Device::open(p).is_ok_and(|d| is_keyboard(&d)))
        .collect()
}

/// Opens new keyboards as they appear (hotplug). Each pass costs a directory
/// listing; a device node is probed only when it is new or was reused after an
/// unplug, so idle CPU stays negligible.
fn watch_devices(sender: Sender<(String, KeyCode, bool)>) {
    let open: Arc<Mutex<HashSet<PathBuf>>> = Default::default();
    // Nodes probed and known not to be keyboards (mice, touchpads).
    let mut seen: HashSet<PathBuf> = HashSet::new();
    loop {
        let nodes: HashSet<PathBuf> = event_nodes().into_iter().collect();
        // Drop vanished nodes so a node number reused by another device is
        // evaluated again. Readers also remove their path on exit.
        seen.retain(|p| nodes.contains(p));
        open.lock().unwrap().retain(|p| nodes.contains(p));
        for path in nodes {
            if seen.contains(&path) || open.lock().unwrap().contains(&path) {
                continue;
            }
            match Device::open(&path) {
                // Unreadable for now (udev ACL not applied yet, node going away):
                // retry next pass.
                Err(_) => {}
                Ok(device) if !is_keyboard(&device) => {
                    seen.insert(path);
                }
                Ok(mut device) => {
                    open.lock().unwrap().insert(path.clone());
                    let (sender, open) = (sender.clone(), open.clone());
                    std::thread::spawn(move || {
                        let id = path.to_string_lossy().to_string();
                        log::info!("hotkey: watching {}", device.name().unwrap_or("keyboard"));
                        'read: while let Ok(events) = device.fetch_events() {
                            for event in events {
                                // value 2 is autorepeat; ignore it.
                                if let EventSummary::Key(_, key, value @ 0..=1) =
                                    event.destructure()
                                {
                                    if sender.send((id.clone(), key, value == 1)).is_err() {
                                        break 'read;
                                    }
                                }
                            }
                        }
                        let _ = sender.send((id, KeyCode::KEY_RESERVED, false));
                        open.lock().unwrap().remove(&path);
                    });
                }
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HotkeyEvent::*;
    use KeyCode as K;

    #[test]
    fn parses_shortcuts() {
        assert_eq!(parse("Ctrl+Super").unwrap().0.len(), 2);
        assert_eq!(parse("ctrl + alt + space").unwrap().0.len(), 3);
        assert_eq!(parse("F9").unwrap().0, vec![vec![K::KEY_F9]]);
        assert!(parse("Ctrl+Bogus").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn press_and_release_in_any_order() {
        let chord = parse("Ctrl+Super").unwrap();
        let mut t = Tracker::new();
        assert_eq!(t.key(&chord, K::KEY_LEFTCTRL, true), None);
        assert_eq!(t.key(&chord, K::KEY_LEFTMETA, true), Some(Pressed));
        assert_eq!(t.key(&chord, K::KEY_LEFTCTRL, false), Some(Released));
        assert_eq!(t.key(&chord, K::KEY_LEFTMETA, false), None);
        assert_eq!(t.key(&chord, K::KEY_RIGHTMETA, true), None);
        assert_eq!(t.key(&chord, K::KEY_RIGHTCTRL, true), Some(Pressed));
        assert_eq!(t.key(&chord, K::KEY_RIGHTMETA, false), Some(Released));
    }

    #[test]
    fn other_key_cancels_until_all_released() {
        let chord = parse("Ctrl+Super").unwrap();
        let mut t = Tracker::new();
        t.key(&chord, K::KEY_LEFTCTRL, true);
        assert_eq!(t.key(&chord, K::KEY_LEFTMETA, true), Some(Pressed));
        assert_eq!(t.key(&chord, K::KEY_LEFT, true), Some(Cancelled));
        assert_eq!(t.key(&chord, K::KEY_LEFT, false), None);
        assert_eq!(t.key(&chord, K::KEY_LEFTMETA, false), None);
        assert_eq!(
            t.key(&chord, K::KEY_LEFTMETA, true),
            None,
            "still cancelled while Ctrl held"
        );
        t.key(&chord, K::KEY_LEFTMETA, false);
        t.key(&chord, K::KEY_LEFTCTRL, false);
        t.key(&chord, K::KEY_LEFTCTRL, true);
        assert_eq!(t.key(&chord, K::KEY_LEFTMETA, true), Some(Pressed));
    }

    #[test]
    fn chord_not_started_when_other_key_already_held() {
        let chord = parse("Ctrl+Super").unwrap();
        let mut t = Tracker::new();
        t.key(&chord, K::KEY_C, true);
        t.key(&chord, K::KEY_LEFTCTRL, true);
        assert_eq!(t.key(&chord, K::KEY_LEFTMETA, true), None);
    }
}
