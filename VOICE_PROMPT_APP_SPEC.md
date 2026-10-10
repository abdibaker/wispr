# Voice Prompt App Specification

## Goal

Build a production-ready Linux voice-prompt desktop app for **Pop!\_OS/COSMIC/Wayland**, similar in interaction to Wispr Flow but specifically for dictating prompts to AI coding agents.

Take ownership of the whole job: research → architecture → implementation → testing → packaging → installation. Do not stop at a prototype or plan.

## Mandatory Stack

Use:

- **Rust** for the native/backend application
- **Tauri** for the desktop application framework
- **React + TypeScript** for the UI
- Prefer Tauri/Rust plugins and native Linux APIs where appropriate
- SQLite for local history unless there is a strong implementation reason otherwise

This architecture is a product decision, not something to reconsider.

Do **not** replace the desktop application with Python, Electron, Qt, GTK, or another framework.

Python may only be used as an optional isolated helper/sidecar if genuinely required for something such as a local ML model. The main application, audio pipeline, hotkey handling, state management, settings, provider clients, history, overlay, and Linux integration must remain Rust + Tauri.

If the repository currently contains a Python implementation from an earlier attempt, inspect it for useful research/behavior, then migrate/rebuild it properly in Rust + Tauri. Remove obsolete Python application code once its useful parts have been incorporated.

## Product Flow

Primary UX:

`hold hotkey → speak → release → transcribe → optional cleanup → insert`

The user focuses T3 Code, Codex, Claude Code, terminal, browser, editor, etc., holds a configurable global push-to-talk shortcut, speaks naturally, releases it, and receives the cleaned prompt in the previously focused text field.

Never automatically press Enter/send by default.

Always provide a reliable clipboard fallback.

Show a small non-focus-stealing overlay for:

- recording + microphone level
- transcribing
- cleaning
- success/error

The app should then disappear back into the background.

## 9Router STT

My OpenAI-compatible 9Router base URL is:

`https://llm.abdibaker.com/v1`

Known transcription API (key from the environment, never inline):

```bash
curl -X POST https://llm.abdibaker.com/v1/audio/transcriptions \
  -H "Authorization: Bearer $NINE_ROUTER_KEY" \
  -F "file=@audio.mp3" \
  -F "model=groq/distil-whisper-large-v3-en" \
  -F "response_format=json"
```

Start with:

`groq/distil-whisper-large-v3-en`

First validate the actual API behavior and response format.

Implement STT behind a Rust provider abstraction so models/providers can later be changed without rewriting the application.

Conceptually:

```rust
trait SpeechProvider {
    async fn transcribe(...) -> Result<Transcript>;
}
```

Design for future local STT such as whisper.cpp without coupling the app to it now.

## Prompt Cleanup

Support:

### Raw

`STT → insert`

### Clean

`STT → cleanup model → insert`

Clean should be the default.

Preferred cleanup model:

**gpt-6-luna with low reasoning effort**, using 9Router if supported.

Verify the actual available model name/API before implementing it.

Cleanup must be conservative:

- remove fillers
- remove meaningless repetition
- resolve obvious false starts
- apply explicit self-corrections
- fix grammar/punctuation
- preserve technical terminology
- preserve filenames, paths, commands and project names
- preserve detailed requirements and constraints

Never:

- invent requirements
- add solutions
- silently change meaning
- unnecessarily summarize detailed instructions

Example:

Spoken:

> okay investigate the authentication tests actually don't modify anything yet just find why they're failing and compare with backend

Expected:

> Investigate why the authentication tests are failing and compare them with the backend. Do not modify anything yet.

Keep cleanup behind its own Rust provider abstraction.

## Linux Integration

Target **Pop!\_OS/COSMIC/Wayland first**.

Research current authoritative approaches for:

- PipeWire microphone capture
- microphone enumeration/selection
- global shortcuts under COSMIC/Wayland
- press-and-hold / key-down and key-up behavior
- remembering the previously focused application
- clipboard
- text insertion under Wayland
- non-focus-stealing Tauri overlay windows
- tray/background operation
- autostart
- notifications
- Linux secret/keyring storage

Do not use old X11 assumptions for Wayland.

Create an `InsertionEngine` abstraction.

Use the best reliable insertion mechanism available on COSMIC/Wayland, with:

`automatic insertion → clipboard fallback`

If Wayland prevents a universal safe insertion method, handle that explicitly rather than using fragile/insecure hacks.

## Rust Architecture

Keep the native application modular.

Use components roughly equivalent to:

```text
AudioCapture
HotkeyManager
SpeechProvider
PromptCleaner
InsertionEngine
OverlayController
VocabularyService
HistoryStore
SettingsStore
SecretStore
```

Prefer Rust for all of these.

Keep React/Tauri frontend code focused primarily on presentation and user interaction. Business logic, credentials, networking, audio processing, provider handling, persistence, and system integration should live primarily in Rust where sensible.

Do not put the whole application logic into React.

## UI

Use **Tauri + React + TypeScript**.

Build a polished minimal settings interface with:

### General

- start on login
- global push-to-talk shortcut
- auto insert / clipboard
- notifications
- sounds

### Speech

- microphone
- 9Router endpoint
- STT model
- language
- connection test

### Prompt Cleanup

- enabled
- provider/model
- reasoning effort
- conservative cleanup options

### Vocabulary

Editable custom technical vocabulary such as:

- 9Router
- T3 Code
- Tailscale
- TanStack
- shadcn
- Coolify
- Codex

Use vocabulary as STT hints where supported and for conservative cleanup correction.

### History

Store locally:

- timestamp
- raw transcript
- cleaned prompt
- mode
- provider/model metadata where useful

Allow copy, reuse, delete, clear, and retention settings.

Do not permanently store recorded audio by default.

### Advanced

- insertion method
- logging
- timeout/fallback settings
- diagnostics

## Reliability

Never lose a successful transcription because another step fails.

Examples:

- cleanup fails → preserve raw transcript
- insertion fails → copy to clipboard
- API timeout → useful recoverable error
- credentials invalid → clear configuration error
- microphone disappears → recover gracefully

Do not log prompt/audio contents by default.

Track useful latency:

`hotkey → recording`

`release → STT`

`STT → cleanup`

`cleanup → insertion`

`release → usable prompt`

Keep idle CPU near zero and memory reasonably low.

## Security

Pay particular attention to:

- Linux secret/keyring storage
- API credentials
- temporary audio cleanup
- restrictive file permissions
- logs
- shell/command injection
- clipboard handling
- dependency security

Do not place credentials in the repository.

## Packaging

Produce an application I can actually install on Pop!\_OS.

At minimum provide a practical **`.deb` package** unless Tauri's current Linux packaging gives a clearly better equivalent.

Also configure:

- desktop launcher
- application icon
- autostart support
- clean uninstall
- release build

Research sandbox implications before considering Flatpak.

## Execution

Work autonomously.

Maintain a task checklist and continue through it.

Research implementation details when necessary, but **do not reconsider the Rust + Tauri + React/TypeScript decision**.

Do not stop after research, scaffolding, UI, or a proof of concept.

Do not end a turn just to tell me what you intend to do next when tools/actions can continue.

Ask me only when genuinely blocked by information only I can provide.

Before completion:

1. inspect any existing implementation
2. migrate/remove the Python desktop implementation if present
3. establish the Rust + Tauri architecture
4. implement the full voice pipeline
5. integrate 9Router STT
6. integrate cleanup
7. implement COSMIC/Wayland hotkey/insertion behavior
8. implement overlay/settings/vocabulary/history
9. implement secure credentials
10. test failure/fallback behavior
11. run Rust formatting/lints/tests
12. run frontend checks/tests
13. build release mode
14. package it
15. install/test it locally where possible
16. fix discovered problems
17. measure basic resource usage and latency

Do not declare success because it merely compiles.

## Definition of Done

I can install the application on Pop!\_OS, configure my 9Router credential, focus an AI-agent prompt field, hold the shortcut, speak:

> okay investigate the authentication tests actually don't modify anything yet just find why they're failing and compare with backend

release it, and receive something equivalent to:

> Investigate why the authentication tests are failing and compare them with the backend. Do not modify anything yet.

It is inserted into the intended application where supported, otherwise reliably copied to clipboard.

The app then returns unobtrusively to the background.

It works again after reboot when autostart is enabled.

Audio is not unnecessarily retained.

## Final Report

When genuinely finished, give me only a concise report containing:

- final architecture
- important COSMIC/Wayland findings
- what was implemented
- insertion strategy
- STT and cleanup integration
- tests/results
- measured resources/latency
- package/install command
- first-run setup
- known limitations
- worthwhile future improvements

Do the implementation, not another implementation proposal. use pkexec when sudo password is needed
