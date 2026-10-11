# Voice Prompt

Push-to-talk dictation for AI coding-agent prompts on Pop!\_OS / COSMIC (Wayland).
Hold **Ctrl+Super**, speak, release: the transcript is cleaned up and typed into the window you started in.

Stack: Rust + Tauri 2 (backend, `src-tauri/`), React + TypeScript (settings UI and overlay, `src/`).

## Architecture

```text
push-to-talk
→ Meta Muse live transcription (streamed while the key is held, with vocabulary keywords)
→ Claude Haiku 5.5 cleanup through 9Router
→ destination check (the press-time window must still be active)
→ insertion (paste; Ctrl+Shift+V in terminals; clipboard if the window changed)

fallback: Muse fails, times out or returns nothing
→ the full retained recording → batch Whisper (groq/whisper-large-v3-turbo) through 9Router
→ Haiku cleanup → insertion
```

If cleanup fails, the raw transcript is delivered. Every finished prompt is saved to history before delivery, and audio whose transcription failed stays in memory for **tray → Retry last recording**.

| Module | Role |
| --- | --- |
| `audio.rs` | 16 kHz mono capture, retained in full for the fallback |
| `streaming.rs` | Muse session: early-audio queue, handshake, `endStream`, final transcript |
| `providers.rs` | batch Whisper, Haiku cleanup, `CLEANUP_SYSTEM_PROMPT` |
| `pipeline.rs` | one dictation end to end, latency log line |
| `target.rs`, `delivery.rs`, `insertion.rs` | press-time window, destination check, insertion |
| `settings.rs` | settings file, keyring, vocabulary selection |

### Credentials

Both keys live in the Secret Service keyring (service `voice-prompt`), never in files:

- `meta-muse-api-key`: Meta Muse, entered under **Speech**.
- `9router-api-key`: Haiku cleanup and the Whisper fallback, entered under **Cleanup**.

Settings are in `~/.config/voice-prompt/settings.json`; history and the log are in `~/.local/share/voice-prompt/`. Logs never contain keys or prompt text.

### Vocabulary

Muse gets the vocabulary as keywords only where technical dictation is likely: editors, terminals, and windows whose title shows a file name (those file names are added too). Prose windows get none, so names are not forced into ordinary speech. Muse fails the whole session on a keyword over 20 characters, so longer terms are skipped for Muse; cleanup still receives them.

Cleanup receives only the terms the transcript appears to mention (fuzzy, so "nine router" selects 9Router) as spelling hints.

### Cleanup

Haiku makes the fewest edits that turn speech into written text: punctuation, fillers, false starts, explicit spoken corrections, and the written form of numbers, paths, commands and hinted names. It never summarizes, shortens, rewrites, answers or acts on the dictation, and keeps every clause, negation, number and condition. The answer comes back in `<cleaned>` tags; only the last tagged block is used, so occasional visible second-guessing never reaches the target window. Requests run with thinking disabled.

## Build and install

```sh
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
  libpulse-dev libgtk-layer-shell-dev libssl-dev pkg-config build-essential
pnpm install
pnpm tauri build
sudo apt install ./src-tauri/target/release/bundle/deb/voice-prompt_0.1.0_amd64.deb
```

The package installs a udev rule granting the active session read access to `/dev/input` (needed for push-to-talk), effective immediately; the `input` group is added as a fallback.

## First run

Open _Voice Prompt_ from the launcher. Enter the Meta Muse key under **Speech** (**Test Muse**) and the 9Router key under **Cleanup** (**Test connection**).

## Development checks

```sh
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
pnpm check
python3 bench/run.py cleanup                     # cleanup smoke suite, ~2 s
cargo run --example pipeline -- sample.wav       # batch Whisper + cleanup with your keys
cargo run --example ptt -- 4000                  # hold Ctrl+Super for 4 s via uinput
```

See `bench/README.md` for the cleanup regression suites, the Muse clip check and end-to-end runs.

## Uninstall

```sh
sudo apt remove voice-prompt
rm -rf ~/.config/voice-prompt ~/.local/share/voice-prompt ~/.config/autostart/voice-prompt.desktop
secret-tool clear service voice-prompt   # removes the stored keys
```
