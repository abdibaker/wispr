# Voice Prompt

Push-to-talk dictation for AI coding-agent prompts on Pop!\_OS / COSMIC (Wayland).
Hold **Ctrl+Super**, speak, release: the transcript is cleaned up and typed into the focused app.

Stack: Rust + Tauri 2 (backend, `src-tauri/`), React + TypeScript (settings UI and overlay, `src/`).

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

Open _Voice Prompt_ from the launcher, then **Speech → API key** (stored in the Secret Service keyring) and **Test connection**.

## Development checks

```sh
cd src-tauri && cargo fmt --check && cargo clippy --all-targets && cargo test
pnpm check
VP_KEY=... cargo run --example pipeline -- sample.wav   # live STT + cleanup
cargo run --example ptt -- 4000                          # hold Ctrl+Super for 4 s via uinput
```

## Uninstall

```sh
sudo apt remove voice-prompt
rm -rf ~/.config/voice-prompt ~/.local/share/voice-prompt ~/.config/autostart/voice-prompt.desktop
secret-tool clear service voice-prompt   # removes the stored API key
```
