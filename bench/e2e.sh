#!/bin/sh
# End-to-end dictation through the real app: `bench/e2e.sh [runs] [clip.wav ...]`.
# A temporary null sink stands in for the microphone, a uinput keyboard holds the shortcut,
# and a GTK window receives the text. The app runs from target/debug with an isolated
# config/data dir (your settings and history are untouched; the keyring key is reused).
# Override settings with VP_CLEANUP_MODEL, VP_REASONING_EFFORT, VP_STT_MODEL, VP_INSERTION_METHOD, VP_LANGUAGE,
# VP_STREAMING (1 = live, 0 = batch only), VP_PROVIDER (deepgram|muse), VP_VOCABULARY (JSON list),
# VP_TARGET_TITLE (window title; "main.rs - wispr" makes the target technical),
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
for key in ("cleanup_model", "reasoning_effort", "stt_model", "insertion_method", "language"):
    if os.environ.get("VP_" + key.upper()):
        user[key] = os.environ["VP_" + key.upper()]
if os.environ.get("VP_STREAMING"):
    user["streaming"] = os.environ["VP_STREAMING"] == "1"
if os.environ.get("VP_PROVIDER"):
    user["streaming_provider"] = os.environ["VP_PROVIDER"]
if os.environ.get("VP_VOCABULARY"):
    user["vocabulary"] = json.loads(os.environ["VP_VOCABULARY"])
user.update(microphone="vp_bench.monitor", autostart=False, sounds=False, notifications=False, history_enabled=True)
pathlib.Path(sys.argv[1]).write_text(json.dumps(user))
PY
python3 target.py gtk "$home/received" & target=$!
sleep 1.5
XDG_CONFIG_HOME="$home/config" XDG_DATA_HOME="$home/data" "$debug/voice-prompt" --background >/dev/null 2>&1 & app=$!
sleep 2
kill -0 "$app" 2>/dev/null || { echo "benchmark app instance exited" >&2; exit 1; }
echo "clips $clips"
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
# App resources over the whole run: peak RSS and CPU seconds (utime + stime).
awk '/VmHWM/ {print "resources peak_rss_kb=" $2}' "/proc/$app/status"
awk -v hz="$(getconf CLK_TCK)" '{print "resources cpu_s=" ($14 + $15) / hz}' "/proc/$app/stat"
# Raw transcripts for WER and term accuracy (`run.py e2e-accuracy`).
python3 -c 'import json,sqlite3,sys; [print("raw "+json.dumps({"stt_model":m,"raw":r,"cleaned":c})) for m,r,c in sqlite3.connect(sys.argv[1]).execute("select stt_model, raw, cleaned from history order by id")]' "$home/data/voice-prompt/history.db"
echo "--- received text:"; cat "$home/received"; echo
