//! InsertionEngine: Wayland virtual keyboard (zwp_virtual_keyboard_v1) + data-control clipboard.
//!
//! Our overlay never takes keyboard focus, so the app the user was typing in stays focused and
//! receives the text. Text is typed through a throwaway keymap that maps each character to its
//! own keycode (the approach wtype uses), so any Unicode and any layout works. Newlines are sent
//! as Shift+Enter so multi-line prompts never auto-submit in chat-style inputs.
use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Method {
    /// Type the text with a virtual keyboard.
    Type,
    /// Put text on the clipboard and press Ctrl+V.
    Paste,
    /// Put text on the clipboard and press Ctrl+Shift+V (terminals).
    PasteTerminal,
    /// Clipboard only.
    Clipboard,
    /// Paste with the shortcut the confirmed window takes; type into unknown or unverified apps.
    Auto,
}

impl Method {
    pub fn parse(name: &str) -> Self {
        match name {
            "paste" => Self::Paste,
            "paste-terminal" => Self::PasteTerminal,
            "clipboard" => Self::Clipboard,
            "auto" => Self::Auto,
            _ => Self::Type,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Type => "typed",
            Self::Paste => "pasted",
            Self::PasteTerminal => "pasted_terminal",
            Self::Clipboard => "copied",
            Self::Auto => "auto",
        }
    }
}

/// The concrete method for a destination. `window` is the confirmed target, or `None`
/// when the compositor offers no window evidence.
pub fn choose(configured: Method, window: Option<&crate::target::Window>, text: &str) -> Method {
    let terminal = window.is_some_and(|w| w.is_terminal());
    match configured {
        // Ctrl+V is literal ^V in a terminal.
        Method::Paste if terminal => Method::PasteTerminal,
        // Typing a newline into a terminal sends Return and runs the line; a bracketed
        // paste (Ctrl+Shift+V) does not.
        Method::Type if terminal && text.contains('\n') => Method::PasteTerminal,
        Method::Auto => match window {
            Some(_) if terminal => Method::PasteTerminal,
            Some(w) if w.accepts_paste() => Method::Paste,
            _ => Method::Type,
        },
        method => method,
    }
}

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    Options::new()
        .copy(Source::Bytes(text.as_bytes().into()), MimeType::Text)
        .map_err(|e| anyhow!("Clipboard unavailable: {e}"))
}

/// Where an insertion spent its time.
#[derive(Clone, Copy, Debug, Default)]
pub struct InsertTiming {
    /// Wayland connection and virtual keyboard creation (plus the paste settle delay).
    pub setup_ms: u64,
    /// Key events: typing every character, or the paste shortcut.
    pub keys_ms: u64,
    /// Publishing the clipboard selection.
    pub clipboard_ms: u64,
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

pub fn insert(method: Method, text: &str) -> Result<InsertTiming> {
    let mut timing = InsertTiming::default();
    let started = Instant::now();
    match method {
        Method::Clipboard | Method::Auto => {
            copy_to_clipboard(text)?;
            timing.clipboard_ms = ms(started);
        }
        Method::Type => {
            let keyboard = VirtualKeyboard::connect()?;
            timing.setup_ms = ms(started);
            let typing = Instant::now();
            keyboard.type_text(text)?;
            timing.keys_ms = ms(typing);
        }
        Method::Paste | Method::PasteTerminal => {
            copy_to_clipboard(text)?;
            timing.clipboard_ms = ms(started);
            let setup = Instant::now();
            // Give the compositor a moment to announce the new selection to the focused client.
            std::thread::sleep(Duration::from_millis(60));
            let keyboard = VirtualKeyboard::connect()?;
            timing.setup_ms = ms(setup);
            let keys = Instant::now();
            keyboard.paste(method == Method::PasteTerminal)?;
            timing.keys_ms = ms(keys);
        }
    }
    Ok(timing)
}

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Keysym for a character in an XKB symbols section.
fn keysym(c: char) -> String {
    match c {
        '\n' => "Return".into(),
        '\t' => "Tab".into(),
        // Placeholder for a keycode left unmapped.
        '\0' => "NoSymbol".into(),
        c => format!("U{:04X}", c as u32),
    }
}

const SHIFT: u32 = 1;
const CTRL: u32 = 4;
/// Evdev keycodes we remap: only the main block's character keys. Chromium and Electron
/// derive the DOM `code` from the keycode, so remapping Esc (1), Backspace (14) or Tab (15)
/// made VS Code and Chrome drop or delete characters. XKB keycode = evdev + 8.
const CODES: [u32; 49] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, // 1 … =
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, // Q … ]
    30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, // A … `
    43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, // \ … /
    57, 86, // Space, 102nd
];

const KEY_V: u32 = 47;

/// Splits text into runs with at most `CODES.len()` distinct characters each; every run is
/// typed with its own keymap.
fn batches(chars: &[char]) -> Vec<&[char]> {
    let mut out = Vec::new();
    let (mut start, mut unique) = (0, Vec::new());
    for (i, c) in chars.iter().enumerate() {
        if !unique.contains(c) {
            if unique.len() == CODES.len() {
                out.push(&chars[start..i]);
                start = i;
                unique.clear();
            }
            unique.push(*c);
        }
    }
    if start < chars.len() {
        out.push(&chars[start..]);
    }
    out
}

pub fn keymap(chars: &[char]) -> String {
    let mut codes = String::new();
    let mut symbols = String::new();
    for (c, evdev) in chars.iter().zip(CODES) {
        let code = evdev + 8;
        codes += &format!("<K{code}> = {code};\n");
        symbols += &format!("key <K{code}> {{ [ {} ] }};\n", keysym(*c));
    }
    let max = CODES[..chars.len().max(1)].iter().max().unwrap() + 8;
    // COSMIC ignores a keymap identical to the previous one, so a client focused since then
    // reads our keycodes with the user's layout ("alpha" typed as "123"). A unique keycodes
    // name makes every keymap distinct.
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let id = format!(
        "vp{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    format!(
        "xkb_keymap {{\nxkb_keycodes \"{id}\" {{ minimum = 8; maximum = {max};\n{codes}}};\n\
         xkb_types \"vp\" {{ include \"complete\" }};\nxkb_compatibility \"vp\" {{ include \"complete\" }};\n\
         xkb_symbols \"vp\" {{\n{symbols}}};\n}};\n"
    )
}

pub struct VirtualKeyboard {
    connection: Connection,
    queue: wayland_client::EventQueue<State>,
    keyboard: ZwpVirtualKeyboardV1,
    start: Instant,
}

impl VirtualKeyboard {
    pub fn connect() -> Result<Self> {
        let connection = Connection::connect_to_env().context("No Wayland session")?;
        let (globals, queue) = registry_queue_init::<State>(&connection)?;
        let handle = queue.handle();
        let seat: WlSeat = globals.bind(&handle, 1..=7, ())?;
        let manager: ZwpVirtualKeyboardManagerV1 =
            globals.bind(&handle, 1..=1, ()).map_err(|_| {
                anyhow!("Compositor does not support virtual keyboards; using clipboard")
            })?;
        let keyboard = manager.create_virtual_keyboard(&seat, &handle, ());
        Ok(Self {
            connection,
            queue,
            keyboard,
            start: Instant::now(),
        })
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn set_keymap(&mut self, chars: &[char]) -> Result<()> {
        let text = keymap(chars);
        let fd = memfd::MemfdOptions::default().create("voice-prompt-keymap")?;
        let mut file = fd.as_file();
        file.write_all(text.as_bytes())?;
        file.write_all(&[0])?;
        // format 1 = xkb_v1
        self.keyboard
            .keymap(1, fd.as_file().as_fd(), text.len() as u32 + 1);
        self.queue.roundtrip(&mut State)?;
        Ok(())
    }

    fn tap(&mut self, code: u32) -> Result<()> {
        self.keyboard.key(self.time(), code, 1);
        self.keyboard.key(self.time(), code, 0);
        self.connection.flush()?;
        // Throttle so slow clients don't drop events from an overfull buffer.
        std::thread::sleep(Duration::from_micros(1500));
        Ok(())
    }

    fn modifiers(&mut self, mask: u32) {
        self.keyboard.modifiers(mask, 0, 0, 0);
    }

    pub fn type_text(mut self, text: &str) -> Result<()> {
        let text = text.replace("\r\n", "\n");
        let chars: Vec<char> = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        for batch in batches(&chars) {
            self.type_batch(batch)?;
        }
        self.queue.roundtrip(&mut State)?;
        self.keyboard.destroy();
        self.connection.flush()?;
        Ok(())
    }

    fn type_batch(&mut self, batch: &[char]) -> Result<()> {
        let mut unique: Vec<char> = Vec::new();
        for c in batch {
            if !unique.contains(c) {
                unique.push(*c);
            }
        }
        self.set_keymap(&unique)?;
        self.modifiers(0);
        for c in batch {
            let code = CODES[unique.iter().position(|u| u == c).unwrap()];
            if *c == '\n' {
                self.modifiers(SHIFT);
                self.tap(code)?;
                self.modifiers(0);
            } else {
                self.tap(code)?;
            }
        }
        Ok(())
    }

    pub fn paste(mut self, shift: bool) -> Result<()> {
        // Electron resolves Ctrl+<key> shortcuts from the physical key, so "v" must sit on
        // the V keycode (evdev 47) for VS Code to see Ctrl+V.
        let v = CODES.iter().position(|&c| c == KEY_V).unwrap();
        let mut keys = vec!['\0'; v + 1];
        keys[v] = 'v';
        self.set_keymap(&keys)?;
        self.modifiers(if shift { CTRL | SHIFT } else { CTRL });
        self.tap(KEY_V)?;
        self.modifiers(0);
        self.queue.roundtrip(&mut State)?;
        self.keyboard.destroy();
        self.connection.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_maps_each_char() {
        let map = keymap(&['a', 'é', '\n']);
        assert!(map.contains("<K10> = 10;"));
        assert!(map.contains("key <K10> { [ U0061 ] };"));
        assert!(map.contains("key <K11> { [ U00E9 ] };"));
        assert!(map.contains("key <K12> { [ Return ] };"));
        assert!(map.contains("maximum = 12;"));
    }

    #[test]
    fn paste_uses_the_v_key() {
        let v = CODES.iter().position(|&c| c == KEY_V).unwrap();
        let mut keys = vec!['\0'; v + 1];
        keys[v] = 'v';
        assert!(keymap(&keys).contains("key <K55> { [ U0076 ] };"));
    }

    #[test]
    fn keycodes_avoid_control_keys() {
        // Esc, Backspace, Tab, Enter, Ctrl, Shift, Alt, Super, CapsLock, arrows.
        for control in [
            1, 14, 15, 28, 29, 42, 54, 56, 58, 97, 100, 103, 105, 106, 108, 125,
        ] {
            assert!(!CODES.contains(&control), "{control}");
        }
        let mut sorted = CODES.to_vec();
        sorted.dedup();
        assert_eq!(sorted.len(), CODES.len());
    }

    #[test]
    fn batches_respect_the_code_pool() {
        let text: Vec<char> = (0..120u32)
            .map(|i| char::from_u32(0x4E00 + i % 70).unwrap())
            .collect();
        let runs = batches(&text);
        assert_eq!(runs.concat(), text);
        for run in &runs {
            let mut unique = run.to_vec();
            unique.sort();
            unique.dedup();
            assert!(unique.len() <= CODES.len());
        }
        assert_eq!(batches(&['a', 'b', 'a']).len(), 1);
        assert!(batches(&[]).is_empty());
    }

    #[test]
    fn keymaps_are_distinct() {
        assert_ne!(keymap(&['a']), keymap(&['a']));
    }

    #[test]
    fn methods_parse() {
        assert_eq!(Method::parse("paste-terminal"), Method::PasteTerminal);
        assert_eq!(Method::parse("auto"), Method::Auto);
        assert_eq!(Method::parse("anything"), Method::Type);
    }

    #[test]
    fn method_follows_destination() {
        let window = |app_id: &str| crate::target::Window {
            id: "1".into(),
            app_id: app_id.into(),
            title: String::new(),
        };
        let term = window("com.system76.CosmicTerm");
        let code = window("code");
        assert_eq!(choose(Method::Auto, Some(&code), "x"), Method::Paste);
        assert_eq!(
            choose(Method::Auto, Some(&term), "x"),
            Method::PasteTerminal
        );
        assert_eq!(choose(Method::Auto, None, "x"), Method::Type);
        assert_eq!(
            choose(Method::Auto, Some(&window("target.py")), "x"),
            Method::Type
        );
        assert_eq!(
            choose(Method::Paste, Some(&term), "x"),
            Method::PasteTerminal
        );
        assert_eq!(choose(Method::Type, Some(&term), "one line"), Method::Type);
        assert_eq!(
            choose(Method::Type, Some(&term), "two\nlines"),
            Method::PasteTerminal
        );
        assert_eq!(
            choose(Method::Type, Some(&code), "two\nlines"),
            Method::Type
        );
        assert_eq!(
            choose(Method::Clipboard, Some(&code), "x"),
            Method::Clipboard
        );
    }
}
