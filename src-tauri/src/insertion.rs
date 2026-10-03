//! InsertionEngine: Wayland virtual keyboard (zwp_virtual_keyboard_v1) + data-control clipboard.
//!
//! Our overlay never takes keyboard focus, so the app the user was typing in stays focused and
//! receives the text. Text is typed through a throwaway keymap that maps each character to its
//! own keycode (the approach wtype uses), so any Unicode and any layout works. Newlines are sent
//! as Shift+Enter so multi-line prompts never auto-submit in chat-style inputs.
use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::os::fd::AsFd;
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
}

impl Method {
    pub fn parse(name: &str) -> Self {
        match name {
            "paste" => Self::Paste,
            "paste-terminal" => Self::PasteTerminal,
            "clipboard" => Self::Clipboard,
            _ => Self::Type,
        }
    }
}

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    Options::new()
        .copy(Source::Bytes(text.as_bytes().into()), MimeType::Text)
        .map_err(|e| anyhow!("Clipboard unavailable: {e}"))
}

pub fn insert(method: Method, text: &str) -> Result<()> {
    match method {
        Method::Clipboard => copy_to_clipboard(text),
        Method::Type => VirtualKeyboard::connect()?.type_text(text),
        Method::Paste | Method::PasteTerminal => {
            copy_to_clipboard(text)?;
            // Give the compositor a moment to announce the new selection to the focused client.
            std::thread::sleep(Duration::from_millis(60));
            VirtualKeyboard::connect()?.paste(method == Method::PasteTerminal)
        }
    }
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
        c => format!("U{:04X}", c as u32),
    }
}

const SHIFT: u32 = 1;
const CTRL: u32 = 4;
/// First evdev keycode we assign; XKB keycode = evdev + 8.
const FIRST: u32 = 1;
/// Characters per keymap; longer texts are typed in batches, each with a fresh keymap.
const BATCH: usize = 200;

pub fn keymap(chars: &[char]) -> String {
    let mut codes = String::new();
    let mut symbols = String::new();
    for (i, c) in chars.iter().enumerate() {
        let code = FIRST + i as u32 + 8;
        codes += &format!("<K{code}> = {code};\n");
        symbols += &format!("key <K{code}> {{ [ {} ] }};\n", keysym(*c));
    }
    let max = FIRST + chars.len() as u32 + 8;
    format!(
        "xkb_keymap {{\nxkb_keycodes \"vp\" {{ minimum = 8; maximum = {max};\n{codes}}};\n\
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
        for batch in chars.chunks(BATCH) {
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
            let code = FIRST + unique.iter().position(|u| u == c).unwrap() as u32;
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
        self.set_keymap(&['v'])?;
        self.modifiers(if shift { CTRL | SHIFT } else { CTRL });
        self.tap(FIRST)?;
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
        assert!(map.contains("<K9> = 9;"));
        assert!(map.contains("key <K9> { [ U0061 ] };"));
        assert!(map.contains("key <K10> { [ U00E9 ] };"));
        assert!(map.contains("key <K11> { [ Return ] };"));
        assert!(map.contains("maximum = 12;"));
    }

    #[test]
    fn methods_parse() {
        assert_eq!(Method::parse("paste-terminal"), Method::PasteTerminal);
        assert_eq!(Method::parse("anything"), Method::Type);
    }
}
