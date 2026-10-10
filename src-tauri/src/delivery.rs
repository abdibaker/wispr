//! Puts a finished prompt where the user wanted it, safely. Text is inserted only into the
//! window that was active at key press; we never move focus back, so a changed window, held
//! keys or a failed insertion get the clipboard instead. Only a failed copy is an error.
use crate::insertion::{self, InsertTiming, Method};
use crate::settings::Settings;
use crate::target::{self, Destination, Window};
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long to wait for the user to let go of every key before copying instead.
const HELD_LIMIT: Duration = Duration::from_secs(3);

/// The desktop as delivery sees it.
pub trait Desktop: Send + Sync {
    /// True while any physical key is down.
    fn keys_held(&self) -> bool;
    /// `None` when the compositor offers no window tracking; `Some(None)` when it does but
    /// no window is active.
    fn active_window(&self) -> Option<Option<Window>>;
    /// Blocking; called off the async runtime.
    fn insert(&self, method: Method, text: &str) -> Result<InsertTiming>;
    fn copy(&self, text: &str) -> Result<()>;
}

/// The real Wayland desktop.
pub struct System {
    /// COSMIC active-window tracking; `None` on compositors without the protocols.
    pub windows: Option<target::Tracker>,
    pub keys_held: Box<dyn Fn() -> bool + Send + Sync>,
}

impl Desktop for System {
    fn keys_held(&self) -> bool {
        (self.keys_held)()
    }

    fn active_window(&self) -> Option<Option<Window>> {
        self.windows.as_ref().map(target::Tracker::active)
    }

    fn insert(&self, method: Method, text: &str) -> Result<InsertTiming> {
        insertion::insert(method, text)
    }

    fn copy(&self, text: &str) -> Result<()> {
        insertion::copy_to_clipboard(text)
    }
}

/// What happened, for the overlay and the `latency` log line.
#[derive(Debug, Default)]
pub struct Delivered {
    /// For the overlay, e.g. "Inserted" or "Window changed — copied, press Ctrl+V".
    pub message: &'static str,
    /// typed | pasted | pasted_terminal | copied_<reason>
    pub delivery: String,
    /// confirmed | changed | unknown; empty when no window check was needed.
    pub destination: &'static str,
    pub held_wait_ms: u64,
    pub insert: InsertTiming,
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// Delivers `text` per `settings`, into `target` (the window active at key press) or the
/// clipboard. Fails only when even the clipboard copy fails.
pub async fn deliver(
    desktop: Arc<dyn Desktop>,
    settings: &Settings,
    text: &str,
    target: Option<&Window>,
) -> Result<Delivered> {
    let mut delivered = Delivered::default();
    let copy = |mut delivered: Delivered, reason: &str, message| -> Result<Delivered> {
        let started = Instant::now();
        let result = desktop.copy(text);
        delivered.insert.clipboard_ms += ms(started);
        delivered.delivery = format!("copied_{reason}");
        delivered.message = message;
        result.map(|()| delivered)
    };
    let configured = settings.insertion_method;
    if !settings.auto_insert || configured == Method::Clipboard {
        return copy(delivered, "setting", "Copied — press Ctrl+V");
    }
    // Typing while the user still holds a modifier would turn letters into shortcuts.
    let waiting = Instant::now();
    let held = loop {
        let held = desktop.keys_held();
        if !held || waiting.elapsed() >= HELD_LIMIT {
            break held;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    delivered.held_wait_ms = ms(waiting);
    if held {
        log::warn!("keys still held after 3 s; copying instead of typing");
        return copy(
            delivered,
            "held_keys",
            "Keys still held — copied, press Ctrl+V",
        );
    }
    // Revalidate last, after every wait, right before keys are sent.
    let now = desktop.active_window();
    let destination = match &now {
        Some(now) => target::revalidate(target, now.as_ref()),
        None => Destination::Unknown,
    };
    delivered.destination = destination.label();
    log::info!(
        "destination {} app={}",
        destination.label(),
        target.map_or("none", |w| w.app_id.as_str())
    );
    let window = match destination {
        Destination::Confirmed(window) => Some(window),
        Destination::Changed { from, to } => {
            log::info!(
                "target changed from {} to {}; copying instead of inserting",
                from.app_id,
                to.map_or("none".into(), |w| w.app_id)
            );
            return copy(
                delivered,
                "target_changed",
                "Window changed — copied, press Ctrl+V",
            );
        }
        // Tracking works but no window was active at press: nothing to insert into.
        Destination::Unknown if now.is_some() => {
            return copy(
                delivered,
                "no_target",
                "No target window — copied, press Ctrl+V",
            );
        }
        // No tracking on this compositor: keep the configured behavior.
        Destination::Unknown => None,
    };
    let method = insertion::choose(configured, window.as_ref(), text);
    let inserting = desktop.clone();
    let owned = text.to_string();
    let result = tokio::task::spawn_blocking(move || inserting.insert(method, &owned)).await?;
    match result {
        Ok(timing) => {
            delivered.insert = timing;
            delivered.delivery = method.name().into();
            delivered.message = "Inserted";
            if settings.keep_in_clipboard && method == Method::Type {
                // The text is already inserted; a clipboard failure here is not a delivery failure.
                let started = Instant::now();
                if let Err(error) = desktop.copy(text) {
                    log::warn!("keep-in-clipboard failed: {error:#}");
                }
                delivered.insert.clipboard_ms += ms(started);
            }
            Ok(delivered)
        }
        Err(error) => {
            log::warn!("insertion failed, copying instead: {error:#}");
            copy(delivered, "insert_failed", "Copied — press Ctrl+V")
        }
    }
}

/// A scripted desktop that records what it was asked to do.
#[cfg(test)]
pub mod fake {
    use super::*;
    use anyhow::anyhow;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct Desktop {
        /// Answers `keys_held` in order; the last answer repeats. Empty: never held.
        pub held: Mutex<Vec<bool>>,
        /// `None`: no window tracking.
        pub active: Option<Option<Window>>,
        pub insert_fails: bool,
        pub copy_fails: bool,
        pub inserted: Mutex<Vec<(Method, String)>>,
        pub copied: Mutex<Vec<String>>,
    }

    impl super::Desktop for Desktop {
        fn keys_held(&self) -> bool {
            let mut held = self.held.lock().unwrap();
            match held.len() {
                0 => false,
                1 => held[0],
                _ => held.remove(0),
            }
        }

        fn active_window(&self) -> Option<Option<Window>> {
            self.active.clone()
        }

        fn insert(&self, method: Method, text: &str) -> Result<InsertTiming> {
            if self.insert_fails {
                return Err(anyhow!("no virtual keyboard"));
            }
            self.inserted.lock().unwrap().push((method, text.into()));
            Ok(InsertTiming::default())
        }

        fn copy(&self, text: &str) -> Result<()> {
            if self.copy_fails {
                return Err(anyhow!("Clipboard unavailable"));
            }
            self.copied.lock().unwrap().push(text.into());
            Ok(())
        }
    }

    pub fn window(id: &str, app_id: &str) -> Window {
        Window {
            id: id.into(),
            app_id: app_id.into(),
            title: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{window, Desktop as Fake};
    use super::*;

    async fn run(desktop: &Arc<Fake>, settings: &Settings, target: Option<&Window>) -> Delivered {
        deliver(desktop.clone(), settings, "hello", target)
            .await
            .unwrap()
    }

    fn inserted(desktop: &Fake) -> Vec<(Method, String)> {
        desktop.inserted.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_changed_window_gets_the_clipboard_never_keystrokes() {
        let desktop = Arc::new(Fake {
            active: Some(Some(window("2", "signal"))),
            ..Default::default()
        });
        let target = window("1", "code");
        let delivered = run(&desktop, &Settings::default(), Some(&target)).await;
        assert_eq!(delivered.delivery, "copied_target_changed");
        assert_eq!(delivered.destination, "changed");
        assert!(inserted(&desktop).is_empty());
        assert_eq!(*desktop.copied.lock().unwrap(), ["hello"]);
    }

    #[tokio::test]
    async fn no_active_window_now_counts_as_changed() {
        let desktop = Arc::new(Fake {
            active: Some(None),
            ..Default::default()
        });
        let target = window("1", "code");
        let delivered = run(&desktop, &Settings::default(), Some(&target)).await;
        assert_eq!(delivered.delivery, "copied_target_changed");
        assert!(inserted(&desktop).is_empty());
    }

    #[tokio::test]
    async fn tracking_without_a_press_time_window_copies() {
        // A retry has no press-time window.
        let desktop = Arc::new(Fake {
            active: Some(Some(window("1", "code"))),
            ..Default::default()
        });
        let delivered = run(&desktop, &Settings::default(), None).await;
        assert_eq!(delivered.delivery, "copied_no_target");
        assert!(inserted(&desktop).is_empty());
    }

    #[tokio::test]
    async fn the_confirmed_window_is_typed_into_and_kept_in_the_clipboard() {
        let desktop = Arc::new(Fake {
            active: Some(Some(window("1", "code"))),
            ..Default::default()
        });
        let target = window("1", "code");
        let delivered = run(&desktop, &Settings::default(), Some(&target)).await;
        assert_eq!(delivered.delivery, "typed");
        assert_eq!(delivered.destination, "confirmed");
        assert_eq!(delivered.message, "Inserted");
        assert_eq!(inserted(&desktop), [(Method::Type, "hello".to_string())]);
        assert_eq!(*desktop.copied.lock().unwrap(), ["hello"]);
    }

    #[tokio::test]
    async fn without_tracking_the_configured_method_is_used() {
        let desktop = Arc::new(Fake::default());
        let settings = Settings {
            insertion_method: Method::Paste,
            ..Default::default()
        };
        let delivered = run(&desktop, &settings, None).await;
        assert_eq!(delivered.destination, "unknown");
        assert_eq!(inserted(&desktop), [(Method::Paste, "hello".to_string())]);
    }

    #[tokio::test]
    async fn waits_for_keys_to_be_released_before_typing() {
        let desktop = Arc::new(Fake {
            held: vec![true, true, false].into(),
            ..Default::default()
        });
        let delivered = run(&desktop, &Settings::default(), None).await;
        assert_eq!(delivered.delivery, "typed");
        assert!(delivered.held_wait_ms >= 40);
    }

    #[tokio::test]
    async fn keys_held_past_the_limit_copy_instead_of_typing() {
        let desktop = Arc::new(Fake {
            held: vec![true].into(),
            active: Some(Some(window("1", "code"))),
            ..Default::default()
        });
        let target = window("1", "code");
        let delivered = run(&desktop, &Settings::default(), Some(&target)).await;
        assert_eq!(delivered.delivery, "copied_held_keys");
        assert!(delivered.held_wait_ms >= HELD_LIMIT.as_millis() as u64);
        assert!(inserted(&desktop).is_empty());
    }

    #[tokio::test]
    async fn a_failed_insertion_falls_back_to_the_clipboard() {
        let desktop = Arc::new(Fake {
            insert_fails: true,
            ..Default::default()
        });
        let delivered = run(&desktop, &Settings::default(), None).await;
        assert_eq!(delivered.delivery, "copied_insert_failed");
        assert_eq!(*desktop.copied.lock().unwrap(), ["hello"]);
    }

    #[tokio::test]
    async fn clipboard_setting_never_types() {
        let desktop = Arc::new(Fake::default());
        let settings = Settings {
            auto_insert: false,
            ..Default::default()
        };
        let delivered = run(&desktop, &settings, None).await;
        assert_eq!(delivered.delivery, "copied_setting");
        assert!(inserted(&desktop).is_empty());
    }

    #[tokio::test]
    async fn only_a_failed_copy_is_an_error() {
        let desktop = Arc::new(Fake {
            insert_fails: true,
            copy_fails: true,
            ..Default::default()
        });
        let result = deliver(desktop, &Settings::default(), "hello", None).await;
        assert!(result.is_err());
    }
}
