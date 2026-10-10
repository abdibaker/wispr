#!/bin/sh
# End-to-end dictation through the real app: `bench/e2e.sh [runs] [clip.wav ...]`.
# A temporary null sink stands in for the microphone, a uinput keyboard holds the shortcut,
# and a GTK window receives the text. The app runs from target/debug with an isolated
# config/data dir (your settings and history are untouched; the keyring key is reused).
# Override settings with VP_CLEANUP_MODEL, VP_REASONING_EFFORT, VP_STT_MODEL, VP_INSERTION_METHOD,
# and the binaries with VP_DEBUG (a directory holding voice-prompt and examples/ptt).
# Prints the app's latency lines. Needs a Wayland session; do not type while it runs.
set -eu
cd "$(dirname "$0")"
runs=${1:-3}; shift || true
clips=${*:-audio/en-self-casual.wav audio/tech-numbers.wav}
debug=${VP_DEBUG:-../src-tauri/target/debug}
# Tauri's single-instance lock would hand our shortcut presses to the running app instead.
if pgrep -x voice-prompt >/dev/null; then echo "quit the running Voice Prompt first" >&2; exit 1; fi
home=$(mktemp -d)
mkdir -p "$home/config/voice-prompt" "$home/data"
module=$(pactl load-module module-null-sink sink_name=vp_bench sink_properties=device.description=vp_bench)
cleanup() {
  kill "${app:-}" "${target:-}" 2>/dev/null || true
  pactl unload-module "$module" 2>/dev/null || true
  rm -rf "$home"
}
trap cleanup EXIT INT TERM
python3 - "$home/config/voice-prompt/settings.json" <<'PY'
import json, pathlib, sys
user = json.loads((pathlib.Path.home() / ".config/voice-prompt/settings.json").read_text())
import os
for key in ("cleanup_model", "reasoning_effort", "stt_model", "insertion_method"):
    if os.environ.get("VP_" + key.upper()):
        user[key] = os.environ["VP_" + key.upper()]
user.update(microphone="vp_bench.monitor", autostart=False, sounds=False, notifications=False, history_enabled=True)
pathlib.Path(sys.argv[1]).write_text(json.dumps(user))
PY
python3 target.py gtk "$home/received" & target=$!
sleep 1.5
XDG_CONFIG_HOME="$home/config" XDG_DATA_HOME="$home/data" "$debug/voice-prompt" --background >/dev/null 2>&1 & app=$!
sleep 2
kill -0 "$app" 2>/dev/null || { echo "benchmark app instance exited" >&2; exit 1; }
for run in $(seq "$runs"); do
  for clip in $clips; do
    hold=$(python3 -c "import wave,sys; w=wave.open(sys.argv[1]); print(int(w.getnframes()/w.getframerate()*1000)+400)" "$clip")
    "$debug/examples/ptt" "$hold" | while read -r line; do
      [ "$line" = pressed ] && paplay --device=vp_bench "$clip" &
    done
    sleep "${VP_GAP:-12}"  # STT + cleanup + typing; stay under Groq's 20 requests/minute (VP_GAP=100 for cold runs)
  done
done
grep -E " latency total=| WARN | outcome=| destination " "$home/data/voice-prompt/voice-prompt.log" || true
echo "--- received text:"; cat "$home/received"; echo
