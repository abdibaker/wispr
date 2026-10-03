# Can wispr use COSMIC's GlobalShortcuts portal instead of /dev/input?

**Verdict (as of 2026): No — not yet.** `xdg-desktop-portal-cosmic` does **not**
implement `org.freedesktop.impl.portal.GlobalShortcuts`. The tracking issue
[pop-os/xdg-desktop-portal-cosmic#4](https://github.com/pop-os/xdg-desktop-portal-cosmic/issues/4)
("feature request: GlobalShortcuts portal") is **still open** — filed 2023-05-13,
last activity 2026-04-08. Keeping the evdev/`/dev/input` reader is currently the
only silent, hold-to-talk-capable mechanism on COSMIC.

**Verified on this machine (Pop!_OS/COSMIC, 2026-10-03):**
`/usr/share/xdg-desktop-portal/portals/cosmic.portal` advertises only
`Access;FileChooser;RemoteDesktop;Screenshot;Settings;ScreenCast`, and
`busctl --user introspect org.freedesktop.impl.portal.desktop.cosmic
/org/freedesktop/portal/desktop` lists exactly those six interfaces — no
GlobalShortcuts.

The comment in `src-tauri/src/hotkey.rs` was *half* right: COSMIC lacks the
portal. But its claim that "the portal has no key-release semantics anyway" was
**inaccurate** — the spec defines both `Activated` **and** `Deactivated`
signals, and push-to-talk was an explicit motivating use case in the portal's
design discussion. The real blocker is backend availability on COSMIC (plus a
portal-UX caveat about modifier-only chords — see §5).

---

## 1. Does xdg-desktop-portal-cosmic implement GlobalShortcuts?

**No.**

- Tracking issue: https://github.com/pop-os/xdg-desktop-portal-cosmic/issues/4
  — state: **open**, created 2023-05-13 by @jokeyrhyme, last updated 2026-04-08,
  13 👍. It is referenced as the blocker by downstream apps (e.g.
  https://github.com/SnosMe/awakened-poe-trade/issues/1746, whose maintainer says
  "Till pop-os/xdg-desktop-portal-cosmic#4 is implemented it will not work").
- Repo source tree (https://github.com/pop-os/xdg-desktop-portal-cosmic, default
  branch `master`): `src/` contains `access.rs`, `file_chooser.rs`,
  `screencast.rs`, `screencast_dialog.rs`, `screencast_thread.rs`,
  `screenshot.rs`, `main.rs`, `wayland/`, `widget/` — there is **no
  `global_shortcuts` module**.
- The `data/cosmic.portal` registration file is installed by the Makefile
  (https://github.com/pop-os/xdg-desktop-portal-cosmic/blob/master/Makefile);
  a code search for `org.freedesktop.impl.portal.GlobalShortcuts` in the repo
  only hits issue #4, not any source file. Confirmed locally: the installed
  `cosmic.portal` `Interfaces=` line does not include GlobalShortcuts.
- Secondary confirmation: the ArchWiki portal-backend matrix
  (https://wiki.archlinux.org/title/XDG_Desktop_Portal) lists
  `xdg-desktop-portal-cosmic` Global shortcuts = **No** (it lists only Access,
  FileChooser, ScreenCast, Screenshot, Settings as supported).

**Why it doesn't exist yet** — from upstream maintainer discussion on issue #4
(Drakulix, cosmic-comp maintainer): the hard part is a private channel for the
portal to register "dynamic shortcuts" with `cosmic-comp`. Options discussed were
a new private Wayland protocol, a compositor D-Bus API, a UNIX socket, adopting
Hyprland's `hyprland-global-shortcuts-v1` protocol
(https://github.com/hyprwm/hyprland-protocols/blob/main/protocols/hyprland-global-shortcuts-v1.xml),
or rewriting `cosmic-comp`'s `config.ron`. Plan: keybindings should be managed by
the portal process (permissions, persistence) with `cosmic-comp` only forwarding
registered events — see also
https://github.com/pop-os/cosmic-comp/pull/400 (runtime-configurable keybindings,
merged 2024-07-01, which Drakulix noted he'd rather see move out of cosmic-comp).

Related but *different* COSMIC work (don't confuse these with the portal):

- https://github.com/pop-os/cosmic-comp/issues/897 "Global Shortcuts" — closed
  2025-04-11 (milestone alpha 7). This was about letting **XWayland** clients
  eavesdrop input for legacy-app compat, implemented by
  https://github.com/pop-os/cosmic-comp/pull/1328 (merged 2025-04-02): cosmic-comp
  forwards raw key/mouse events to Xwayland. Known caveat:
  https://github.com/pop-os/cosmic-comp/issues/1390 (X11 apps miss key-release
  events when a compositor-level shortcut fires — same behaviour as KDE).

## 2. What the GlobalShortcuts spec actually provides

Spec XML: https://github.com/flatpak/xdg-desktop-portal/blob/main/data/org.freedesktop.portal.GlobalShortcuts.xml
Frontend docs: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html
Backend docs:  https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.impl.portal.GlobalShortcuts.html

- Introduced in **xdg-desktop-portal 1.18.0** (2023-09-18):
  https://github.com/flatpak/xdg-desktop-portal/releases/tag/1.18.0 /
  https://github.com/flatpak/xdg-desktop-portal/blob/main/NEWS.md
- Interface is at **version 2**; v2 added `ConfigureShortcuts`.
- Flow: `CreateSession(options)` → session object (options:
  `handle_token`, `session_handle_token`; note both tokens are de-facto required —
  https://github.com/flatpak/xdg-desktop-portal/issues/1314) →
  `BindShortcuts(session, shortcuts a(sa{sv}), parent_window, options)` →
  `ListShortcuts`, `ConfigureShortcuts` (v2), plus a `ShortcutsChanged` signal.
- Each shortcut = `(id, {description, preferred_trigger})`; `preferred_trigger`
  uses the XDG shortcuts spec grammar (`CTRL+ALT+Return` style —
  https://specifications.freedesktop.org/shortcuts/latest/). Results are a
  **subset** of what was requested; the backend decides what was actually bound
  and returns `trigger_description`.
- **Release semantics exist**: `Activated(session_handle, shortcut_id,
  timestamp, options)` and `Deactivated(session_handle, shortcut_id, timestamp,
  options)` — "emitted, respectively, whenever a shortcut is activated and
  deactivated." Timestamps are ms-granularity with an undefined base. PTT was an
  explicit design goal: https://github.com/flatpak/xdg-desktop-portal/issues/624
  (KDE's aleixpol: "We need … Push-To-Talk … we'd need handling the release").
- **Caveats:**
  - `BindShortcuts` "will typically result in the portal presenting a dialog
    showing the shortcuts and allowing users to configure the shortcuts", and can
    only be attempted **once per session**. There is no unbind method.
  - Persistence is backend-defined. KDE persists bindings per app-id +
    `session_handle_token` and silently re-arms them on `CreateSession`;
    Hyprland re-shows the dialog each `BindShortcuts` call
    (https://github.com/hyprwm/xdg-desktop-portal-hyprland/pull/241). On KDE,
    `ListShortcuts` on a fresh session returns shortcuts bound in a previous
    session by the same app, enabling silent re-acquisition.
  - Whether `Deactivated` fires reliably is backend-dependent in practice —
    other PTT apps report it as unreliable on some stacks
    (https://github.com/Kieirra/murmure/discussions/305).
  - The `options` vardict on the signals is essentially undocumented
    (https://github.com/flatpak/xdg-desktop-portal/issues/1312); docs are sparse
    overall (https://github.com/flatpak/xdg-desktop-portal/issues/971).
  - Trigger grammar appears to require modifiers **+ a key identifier**; a
    modifier-only chord like "Ctrl+Super" may not be expressible as a
    `preferred_trigger`. Unverified against implementations — check per-backend
    before relying on it.

### Who implements it today

| Backend | Status |
|---|---|
| xdg-desktop-portal-kde (Plasma 6) | Yes — original implementation; advertised in `data/kde.portal` (https://invent.kde.org/plasma/xdg-desktop-portal-kde/-/blob/master/data/kde.portal) |
| xdg-desktop-portal-gnome | Yes — merged during the GNOME 48 cycle (This Week in GNOME #189, https://discourse.gnome.org/t/189-global-shortcuts/27375) |
| xdg-desktop-portal-hyprland | Yes — via `hyprland-global-shortcuts-v1` protocol |
| xdg-desktop-portal-wlr | No — https://github.com/emersion/xdg-desktop-portal-wlr/issues/240 |
| **xdg-desktop-portal-cosmic** | **No — https://github.com/pop-os/xdg-desktop-portal-cosmic/issues/4 (open)** |

## 3. ashpd support

- `ashpd::desktop::global_shortcuts` exists, gated behind the `global_shortcuts`
  cargo feature (included in `frontend`). Docs:
  https://docs.rs/ashpd/latest/ashpd/desktop/global_shortcuts/index.html
- **Added in ashpd 0.4.0** (released 2023-03-24):
  https://github.com/bilelmoussaoui/ashpd/releases/tag/0.4.0
  ("Add global shortcuts implementation").
- API surface (source:
  https://bilelmoussaoui.github.io/ashpd/src/ashpd/desktop/global_shortcuts.rs.html):
  `GlobalShortcuts::new()`, `create_session() -> Session`,
  `bind_shortcuts(&session, shortcuts, window_id)`, `list_shortcuts`,
  `configure_shortcuts`, and **both signal streams** —
  `receive_activated()` / `receive_deactivated()` returning
  `impl Stream<Item = Activated|Deactivated>` with `session_handle()`,
  `shortcut_id()`, `timestamp()` accessors. So ashpd fully supports the
  press/release model.
- For an unsandboxed app like this one, correct app-ID detection matters for
  binding persistence; see ashpd's `AppID`/host-registration handling.

## 4. COSMIC today — summary of the gap

- `xdg-desktop-portal-cosmic` implements: **Access, FileChooser, RemoteDesktop,
  ScreenCast, Screenshot, Settings** (ArchWiki matrix; corroborated by `src/`
  modules and by the installed `cosmic.portal` + live D-Bus introspection on
  this machine). No GlobalShortcuts, no InputCapture.
- Tracking issue #4 open since 2023-05-13; upstream intends to implement it as
  part of COSMIC's keybinding story but needs a private compositor channel that
  doesn't exist yet. No PR implementing it found as of this research.

## 5. Practical implications for wispr

The current evdev reader (src-tauri/src/hotkey.rs) remains the pragmatic choice
on COSMIC:

- **Keep evdev for now.** It's the only mechanism that (a) works on COSMIC today,
  (b) gives true press+release edges for hold-to-talk, (c) is silent (no dialog),
  and (d) supports modifier-only chords like the default `Ctrl+Super`. Cost:
  `input` group membership (effectively keylogger-level trust) — acceptable for a
  personal app; already documented/packaged.
- **Portal path, when/if it lands** (ashpd `global_shortcuts` feature): adds a
  one-time user-facing bind dialog (`BindShortcuts`), the *user* picks the actual
  trigger (the app only suggests `preferred_trigger`), one bind per session, and
  persistence is backend-specific. Also note the likely inability to express a
  modifier-only trigger — wispr's default chord is `Ctrl+Super`, which may need a
  real key (e.g. `Ctrl+Super+Space`) under the portal model anyway.
- **Tauri ecosystem**: `tauri-plugin-global-shortcut`/`global-hotkey` have no
  Wayland support yet; portal-based PRs exist —
  https://github.com/tauri-apps/global-hotkey/pull/162 and
  https://github.com/tauri-apps/global-hotkey/pull/172 (Mar 2026) — unmerged.
- **XWayland fallback**: since cosmic-comp PR #1328 (merged 2025-04-02,
  https://github.com/pop-os/cosmic-comp/pull/1328), X11/XWayland clients on
  COSMIC receive raw input events and can implement global hotkeys — this is how
  e.g. Discord PTT works under COSMIC via Xwayland. Caveats: requires running
  the app (or a helper) as an X11 client, and release events can be dropped when
  a compositor-level shortcut is involved
  (https://github.com/pop-os/cosmic-comp/issues/1390).
- **Compositor-level config**: users can bind a key in COSMIC Settings →
  Keyboard → Custom Shortcuts to run a command (e.g. the app's CLI/D-Bus
  trigger). Works today but is press-only — gives toggle-to-talk, not
  hold-to-talk — and requires manual user setup.
- **GNOME/wlr portals**: not viable on COSMIC (`UseIn=` matching; neither would
  serve cosmic-comp anyway).
