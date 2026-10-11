# Benchmarks

Stdlib Python; uses the app's endpoint (`~/.config/voice-prompt/settings.json`) and keyring key.

- `python3 bench/run.py synth`: regenerate the espeak-ng clips in `audio/` (`synthetic: true` in `corpus.json`).
- `python3 bench/run.py stt [--trials 3] [--hints ...] [--models ...]`: Whisper models × vocabulary hints. `contextual` mirrors the app (extended hint for technical clips only); `contam` counts prose transcripts that gained vocabulary terms. Paced at 3.5 s per request because Groq allows 20 requests/minute per model.
- `python3 bench/run.py cleanup [--trials 2] [--only PATH:MODEL:EFFORT...] [--prompt NAME] [--limit dynamic|fixed|none] [--temperature T] [--app-instructions]`: cleanup through `router` (9Router, token saver off) or `groq` (api.groq.com, keyring `groq-api-key`) against a raw baseline, scored by deterministic preservation checks. p50/p95 are wall time; errors and 429s count as failures, never as fast results (`--tries 1` by default, as the app). Prompt variants live in `prompts.json`; `app` is the shipped `CLEANUP_SYSTEM_PROMPT`. Router prompt tokens are reported minus 9Router's fixed 2,000-token pad.
- `python3 bench/run.py connection [--rounds 6] [--idle 100]`: cold, idle-expired and warm request latency per cleanup path.
- `python3 bench/run.py latency [LOG]`: outcome/delivery/destination counts, then p50/p95 per stage from the app's `latency` log lines (cleaned dictations only), split cold (idle > 90 s) and warm.
- `bench/insertion.sh [trials]`: type vs paste into a GTK text view and a VTE terminal.
- `bench/targets.sh [trials]`: `auto` and `type` insertion into VS Code, a Chrome field, a Chrome chat composer, COSMIC Terminal and a GTK text view, plus a window-change case that must copy instead of insert. Throwaway profiles; `VP_TARGETS` selects targets.
- `bench/e2e.sh [runs] [clips]`: the real app end to end (null-sink microphone, uinput shortcut, GTK target). Quit the running app first.

Real clips: LibriVox (public domain) and the project author's recordings (CC0). Results land in `results/`.
