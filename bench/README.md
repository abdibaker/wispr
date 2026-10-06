# Benchmarks

Stdlib Python; uses the app's endpoint (`~/.config/voice-prompt/settings.json`) and keyring key.

- `python3 bench/run.py synth`: regenerate the espeak-ng clips in `audio/` (`synthetic: true` in `corpus.json`).
- `python3 bench/run.py stt [--trials 3]`: Whisper models × vocabulary hints. Paced at 3.5 s per request because Groq allows 20 requests/minute per model.
- `python3 bench/run.py cleanup [--trials 2] [--only MODEL...] [--pace S]`: cleanup configurations against a raw (no cleanup) baseline, scored by deterministic preservation checks.
- `python3 bench/run.py latency [LOG]`: p50/p95 per stage from the app's `latency` log lines, split cold (idle > 90 s) and warm.
- `bench/insertion.sh [trials]`: type vs paste into a GTK text view and a VTE terminal.
- `bench/e2e.sh [runs] [clips]`: the real app end to end (null-sink microphone, uinput shortcut, GTK target). Quit the running app first.

Real clips: LibriVox (public domain) and the project author's recordings (CC0). Results land in `results/`.
