# Voice Prompt

Push-to-talk dictation for AI coding-agent prompts on Pop!\_OS / COSMIC (Wayland).
Hold **Ctrl+Super**, speak, release: the transcript is cleaned up and typed into the window you started in.

Stack: Rust + Tauri 2 (backend, `src-tauri/`), React + TypeScript (settings UI and overlay, `src/`).

## Architecture

```text
push-to-talk
→ Meta Muse live transcription (streamed while the key is held, with vocabulary keywords)
→ Qwen 3.8 27B cleanup on Cerebras (direct)
→ destination check (the press-time window must still be active)
→ insertion (paste; Ctrl+Shift+V in terminals; clipboard if the window changed)

fallback: Muse fails, times out or returns nothing
→ the full retained recording → batch Whisper (groq/whisper-large-v3-turbo) through 9Router
→ Qwen cleanup → insertion
```

If cleanup fails, the raw transcript is delivered. Every finished prompt is saved to history before delivery, and audio whose transcription failed stays in memory for **tray → Retry last recording**.

| Module | Role |
| --- | --- |
| `audio.rs` | 16 kHz mono capture, retained in full for the fallback |
| `streaming.rs` | Muse session: early-audio queue, handshake, `endStream`, final transcript |
| `providers.rs` | batch Whisper, Qwen cleanup, `CLEANUP_SYSTEM_PROMPT` |
| `pipeline.rs` | one dictation end to end, latency log line |
| `target.rs`, `delivery.rs`, `insertion.rs` | press-time window, destination check, insertion |
| `settings.rs` | settings file, keyring, vocabulary selection |

### Credentials

Both keys live in the Secret Service keyring (service `voice-prompt`), never in files:

- `meta-muse-api-key`: Meta Muse, entered under **Speech**.
- `cerebras-api-key`: Qwen cleanup, entered under **Cleanup**.
- `9router-api-key`: the Whisper fallback, entered under **Cleanup**.

Settings are in `~/.config/voice-prompt/settings.json`; history and the log are in `~/.local/share/voice-prompt/`. Logs never contain keys or prompt text.

### Vocabulary

Muse gets the vocabulary as keywords only where technical dictation is likely: editors, terminals, and windows whose title shows a file name (those file names are added too). Prose windows get none, so names are not forced into ordinary speech. Muse fails the whole session on a keyword over 20 characters, so longer terms are skipped for Muse; cleanup still receives them.

Cleanup receives only the terms the transcript appears to mention (fuzzy, so "nine router" selects 9Router) as spelling hints.

### Cleanup

Qwen makes the fewest edits that turn speech into written text: punctuation, fillers, false starts, explicit spoken corrections, and the written form of numbers, paths, commands and hinted names. It never summarizes, shortens, rewrites, answers or acts on the dictation, and keeps every clause, negation, number and condition. The answer comes back in `<cleaned>` tags; only the last tagged block is used, so occasional visible second-guessing never reaches the target window. Requests run with thinking disabled.

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

## Verification

Run the completion gate from the repository root:

```sh
pnpm verify
```

The gate runs its own failure-propagation self-check, Rust formatting, all-target
Clippy with warnings denied, Rust library/binary/doc tests, the Python evaluator
self-check, and the frontend typecheck and production build. It stops at the first
failure and prints per-stage and total elapsed time.

Prerequisites: the Linux development packages listed above, Bash, Python 3.9+,
Rust with `rustfmt` and Clippy, and installed frontend dependencies. Cargo runs
with `--locked --offline` and reuses its normal build cache. On a fresh checkout,
prepare dependencies with `pnpm install --frozen-lockfile` and
`cargo fetch --locked --manifest-path src-tauri/Cargo.toml` before verification.

The gate does not fetch dependencies, call live providers, access API credentials,
capture audio or send desktop input. It uses local loopback servers in tests.
It builds `dist/`, but skips release compilation, packaging and live benchmarks.

### Fast iteration

Run only the affected check while editing, then `pnpm verify` before handoff.
All commands below run from the repository root:

| Changed area | Focused check |
| --- | --- |
| Rust subsystem | `cargo test --locked --offline --manifest-path src-tauri/Cargo.toml --lib streaming::tests` (replace `streaming` with the affected module) |
| App orchestration | `cargo test --locked --offline --manifest-path src-tauri/Cargo.toml --bin voice-prompt` |
| TypeScript | `pnpm check` |
| Cleanup evaluator | `python3 bench/evaluate.py` |
| Verification script | `python3 scripts/test_verify.py` |

UI changes also need a real-browser or app check. Microphone capture, Wayland
delivery and live provider quality/latency need separate opt-in checks; passing
the local gate does not prove them. See `bench/README.md` before running those
checks. Its cleanup runner still targets historical Haiku through 9Router, not
the production Qwen/Cerebras path.

## Uninstall

```sh
sudo apt remove voice-prompt
rm -rf ~/.config/voice-prompt ~/.local/share/voice-prompt ~/.config/autostart/voice-prompt.desktop
secret-tool clear service voice-prompt   # removes the stored keys
```
