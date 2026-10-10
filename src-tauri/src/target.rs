//! Destination check: which window was active at key press, and is it still active now?
//!
//! COSMIC exposes window activation through ext_foreign_toplevel_list_v1 plus its
//! zcosmic_toplevel_info_v1 (v2+) state extension. A background thread keeps the active
//! window current, so a snapshot is a mutex read. This validates the window, not the
//! focused field inside it. We never re-activate a window: a changed target means the
//! text goes to the clipboard instead.
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};

#[allow(
    dead_code,
    non_upper_case_globals,
    non_camel_case_types,
    unused_imports
)]
mod protocol {
    /// Interface-less object args (the patched deprecated workspace events) are typed
    /// `wayland_client::ObjectId` by the scanner; the crate only exports it from `backend`.
    pub mod wayland_client {
        pub use ::wayland_client::backend::ObjectId;
        pub use ::wayland_client::*;
    }
    use wayland_client::protocol::*;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::*;
    use wayland_protocols::ext::workspace::v1::client::*;
    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces::*;
        use wayland_protocols::ext::workspace::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/cosmic-toplevel-info-unstable-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/cosmic-toplevel-info-unstable-v1.xml");
}
use protocol::{
    zcosmic_toplevel_handle_v1::{self, ZcosmicToplevelHandleV1},
    zcosmic_toplevel_info_v1::{self, ZcosmicToplevelInfoV1},
};

/// The active toplevel window.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Window {
    /// Compositor-unique for the window's lifetime (ext-foreign-toplevel `identifier`).
    pub id: String,
    pub app_id: String,
    pub title: String,
}

impl Window {
    /// Terminals take Ctrl+Shift+V, and a pasted newline may execute a command.
    pub fn is_terminal(&self) -> bool {
        let app = self.app_id.to_lowercase();
        [
            "term",
            "alacritty",
            "kitty",
            "foot",
            "konsole",
            "wezterm",
            "ghostty",
            "tilix",
            "xterm",
        ]
        .iter()
        .any(|name| app.contains(name))
    }

    /// Apps verified to accept a virtual-keyboard paste shortcut on COSMIC. GTK3 apps ignore
    /// synthesized Ctrl shortcuts here (typing still works), so unknown apps are typed.
    pub fn accepts_paste(&self) -> bool {
        let app = self.app_id.to_lowercase();
        self.is_terminal()
            || app == "code"
            || app == "codium"
            || app.starts_with("com.system76.cosmic")
            || app.starts_with("google-chrome")
            || app.starts_with("chrome-")
            || app.starts_with("chromium")
            || app.starts_with("brave")
            || app.starts_with("com.t3tools.")
    }

    /// Editors and terminals: dictation there is likely technical.
    pub fn is_technical(&self) -> bool {
        let app = self.app_id.to_lowercase();
        self.is_terminal()
            || [
                "code",
                "codium",
                "zed",
                "jetbrains",
                "idea",
                "neovim",
                "nvim",
                "vim",
                "emacs",
                "cosmic-edit",
                "gedit",
                "sublime",
                "kate",
                "helix",
                "t3",
            ]
            .iter()
            .any(|name| app.contains(name))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Destination {
    /// The window active at key press is still active.
    Confirmed(Window),
    /// Another window (or none) is active now.
    Changed { from: Window, to: Option<Window> },
    /// No window evidence: the compositor lacks the protocols, or nothing was active.
    Unknown,
}

impl Destination {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Confirmed(_) => "confirmed",
            Self::Changed { .. } => "changed",
            Self::Unknown => "unknown",
        }
    }
}

/// Compares the press-time snapshot with the current window.
pub fn revalidate(at_press: Option<&Window>, now: Option<&Window>) -> Destination {
    match (at_press, now) {
        (Some(from), Some(to)) if from.id == to.id => Destination::Confirmed(to.clone()),
        (Some(from), to) => Destination::Changed {
            from: from.clone(),
            to: to.cloned(),
        },
        (None, _) => Destination::Unknown,
    }
}

#[derive(Clone)]
pub struct Tracker {
    active: Arc<Mutex<Option<Window>>>,
}

impl Tracker {
    /// Connects and starts tracking; fails when the compositor lacks the protocols.
    pub fn start() -> Result<Self> {
        let connection = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
        let handle = queue.handle();
        let _list: ExtForeignToplevelListV1 = globals
            .bind(&handle, 1..=1, ())
            .map_err(|_| anyhow!("no ext_foreign_toplevel_list_v1"))?;
        // v2 is the first version that extends ext-foreign-toplevel handles.
        let info: ZcosmicToplevelInfoV1 = globals
            .bind(&handle, 2..=3, ())
            .map_err(|_| anyhow!("no zcosmic_toplevel_info_v1 v2+"))?;
        let active = Arc::new(Mutex::new(None));
        let mut state = State {
            info,
            windows: HashMap::new(),
            active: active.clone(),
        };
        // Toplevels are announced, then their COSMIC handles are requested; COSMIC sends the
        // initial states some time after that, not within a fixed number of roundtrips.
        let ready = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while active.lock().unwrap().is_none() && std::time::Instant::now() < ready {
            queue.roundtrip(&mut state)?;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::Builder::new()
            .name("target-tracker".into())
            .spawn(move || loop {
                if let Err(error) = queue.blocking_dispatch(&mut state) {
                    log::warn!("window tracking stopped: {error}");
                    *state.active.lock().unwrap() = None;
                    return;
                }
            })?;
        Ok(Self { active })
    }

    pub fn active(&self) -> Option<Window> {
        self.active.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct Toplevel {
    window: Window,
    /// Committed at zcosmic_toplevel_info_v1 `done`, so activation changes across windows
    /// are seen atomically.
    activated: bool,
    pending_activated: bool,
    cosmic: Option<ZcosmicToplevelHandleV1>,
}

struct State {
    info: ZcosmicToplevelInfoV1,
    windows: HashMap<wayland_client::backend::ObjectId, Toplevel>,
    active: Arc<Mutex<Option<Window>>>,
}

impl State {
    fn publish(&self) {
        let active = self
            .windows
            .values()
            .find(|t| t.activated)
            .map(|t| t.window.clone());
        *self.active.lock().unwrap() = active;
    }
}

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

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            let cosmic = state
                .info
                .get_cosmic_toplevel(&toplevel, handle, toplevel.id());
            state.windows.insert(
                toplevel.id(),
                Toplevel {
                    cosmic: Some(cosmic),
                    ..Default::default()
                },
            );
        }
    }

    event_created_child!(State, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        let id = proxy.id();
        match event {
            Event::Closed => {
                if let Some(toplevel) = state.windows.remove(&id) {
                    if let Some(cosmic) = toplevel.cosmic {
                        cosmic.destroy();
                    }
                }
                proxy.destroy();
                state.publish();
            }
            Event::Identifier { identifier } => {
                if let Some(t) = state.windows.get_mut(&id) {
                    t.window.id = identifier;
                }
            }
            Event::AppId { app_id } => {
                if let Some(t) = state.windows.get_mut(&id) {
                    t.window.app_id = app_id;
                }
            }
            Event::Title { title } => {
                if let Some(t) = state.windows.get_mut(&id) {
                    t.window.title = title;
                }
            }
            Event::Done => state.publish(),
            _ => {}
        }
    }
}

impl Dispatch<ZcosmicToplevelInfoV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicToplevelInfoV1,
        event: zcosmic_toplevel_info_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_info_v1::Event::Done = event {
            for t in state.windows.values_mut() {
                t.activated = t.pending_activated;
            }
            state.publish();
        }
    }
}

/// User data: the ext-foreign-toplevel handle this extension belongs to.
impl Dispatch<ZcosmicToplevelHandleV1, wayland_client::backend::ObjectId> for State {
    fn event(
        state: &mut Self,
        _: &ZcosmicToplevelHandleV1,
        event: zcosmic_toplevel_handle_v1::Event,
        foreign: &wayland_client::backend::ObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zcosmic_toplevel_handle_v1::Event::State { state: states } = event {
            let activated = states.as_chunks::<4>().0.iter().any(|s| {
                u32::from_ne_bytes(*s) == zcosmic_toplevel_handle_v1::State::Activated as u32
            });
            if let Some(t) = state.windows.get_mut(foreign) {
                t.pending_activated = activated;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, app_id: &str) -> Window {
        Window {
            id: id.into(),
            app_id: app_id.into(),
            title: String::new(),
        }
    }

    #[test]
    fn revalidation_distinguishes_outcomes() {
        let code = window("1", "code");
        let term = window("2", "com.system76.CosmicTerm");
        assert_eq!(revalidate(Some(&code), Some(&code)).label(), "confirmed");
        assert_eq!(revalidate(Some(&code), Some(&term)).label(), "changed");
        assert_eq!(revalidate(Some(&code), None).label(), "changed");
        assert_eq!(revalidate(None, Some(&code)).label(), "unknown");
        // Same app, different window is still a change.
        assert_eq!(
            revalidate(Some(&code), Some(&window("3", "code"))).label(),
            "changed"
        );
    }

    #[test]
    fn app_classes() {
        assert!(window("1", "com.system76.CosmicTerm").is_terminal());
        assert!(window("1", "org.gnome.Terminal").is_terminal());
        assert!(!window("1", "code").is_terminal());
        assert!(window("1", "code").is_technical());
        assert!(!window("1", "firefox").is_technical());
        assert!(!window("1", "google-chrome").is_technical());
        assert!(window("1", "code").accepts_paste());
        assert!(window("1", "chrome-127.0.0.1__-Default").accepts_paste());
        assert!(window("1", "com.system76.CosmicEdit").accepts_paste());
        assert!(!window("1", "target.py").accepts_paste());
        assert!(!window("1", "org.gnome.TextEditor").accepts_paste());
    }
}
