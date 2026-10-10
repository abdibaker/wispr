#!/bin/sh
# Native insertion across real targets: `bench/targets.sh [trials]`. Each target opens in a
# throwaway profile and must take focus (do not touch the keyboard meanwhile). Prints one JSON
# line per insertion with exactness. Targets: VS Code, Chrome textarea, Chrome chat composer
# (Enter sends), COSMIC Terminal, GTK text view. Last, a target-change case: the window
# changes between snapshot and delivery and the text must be copied, not inserted.
set -eu
cd "$(dirname "$0")"
bin=${VP_DEBUG:-../src-tauri/target/debug}/examples/inserttest
trials=${1:-2}
dir=$(mktemp -d)
trap 'pkill -f "$dir" 2>/dev/null || true; rm -rf "$dir"' EXIT INT TERM
printf 'pnpm check && cargo test --release' > "$dir/short"
printf 'fn main() {\n    let path = "src-tauri/Cargo.toml"; // keep — ünïcode ✓ 日本\n        indented();\n}' > "$dir/multiline"
python3 -c "print(('Update the retry logic in providers.rs, keep 3 attempts and a 250 ms backoff; never log the API key. ' * 20).strip(), end='')" > "$dir/long"
printf '#!/bin/sh\nstty raw -echo\nexec cat > "$VP_OUT.raw"\n' > "$dir/capture-shell"
chmod +x "$dir/capture-shell"

open_target() {  # name out
  case $1 in
    vscode)
      mkdir -p "$dir/code-user"
      printf '{"workbench.startupEditor":"none","editor.autoClosingBrackets":"never","editor.autoClosingQuotes":"never","editor.autoIndent":"none","editor.formatOnPaste":false,"editor.acceptSuggestionOnEnter":"off","editor.quickSuggestions":{"other":false,"comments":false,"strings":false},"window.restoreWindows":"none","files.autoSave":"afterDelay","files.autoSaveDelay":200,"security.workspace.trust.enabled":false}' \
        > "$dir/code-user/settings.json"
      mkdir -p "$dir/code-user/User" && cp "$dir/code-user/settings.json" "$dir/code-user/User/"
      : > "$2"
      code --user-data-dir "$dir/code-user" --extensions-dir "$dir/code-ext" --disable-extensions -n "$2" >/dev/null 2>&1 &
      sleep 6 ;;
    chrome-field) python3 webtarget.py field "$2" & pid=$!; sleep 4 ;;
    chrome-chat) python3 webtarget.py chat "$2" & pid=$!; sleep 4 ;;
    cosmic-term) VP_OUT=$2 SHELL=$dir/capture-shell cosmic-term & pid=$!; sleep 2.5 ;;
    gtk) python3 target.py gtk "$2" & pid=$!; sleep 1.5 ;;
  esac
}

close_target() {  # name
  case $1 in
    vscode) pkill -f "$dir/code-user" || true ;;
    chrome-*) pkill -f vp-chrome- || true; kill "$pid" 2>/dev/null || true ;;
    *) kill "$pid" 2>/dev/null || true ;;
  esac
  sleep 0.8
}

check() {  # target method sample expected out timing
  python3 - "$@" <<'PY'
import json, sys
target, method, sample, expected_path, out, timing = sys.argv[1:]
expected = open(expected_path).read()
got = ""
for path in (out + ".raw", out):
    try:
        got = open(path, "rb").read().decode("utf-8", "replace")
        break
    except FileNotFoundError:
        pass
sent = ""
if target.startswith("chrome"):
    sent, _, got = got.partition("\n---\n")
if target == "cosmic-term":
    # Bracketed paste markers wrap a paste; a typed newline arrives as CR.
    got = got.replace("\x1b[200~", "").replace("\x1b[201~", "").replace("\r", "\n")
try:
    t = json.loads(timing)
except ValueError:
    t = {"error": timing.strip()[:160]}
print(json.dumps({"target": target, "configured": method, "sample": sample, "chars": len(expected),
                  "exact": got == expected, "got_chars": len(got), "submitted": bool(sent), **t}), flush=True)
PY
}

for target in ${VP_TARGETS:-vscode chrome-field chrome-chat cosmic-term gtk}; do
  for method in auto type; do
    for sample in short multiline long; do
      for trial in $(seq "$trials"); do
        out="$dir/$target-$method-$sample-$trial"
        open_target "$target" "$out"
        timing=$("$bin" "$method" < "$dir/$sample" 2>&1 || true)
        # VS Code autosaves after 200 ms; typing long text takes longer.
        sleep 2
        close_target "$target"
        check "$target" "$method" "$sample" "$dir/$sample" "$out" "$timing"
      done
    done
  done
done

# Target change: snapshot GTK window A, then open window B before delivery.
for trial in $(seq "$trials"); do
  python3 target.py gtk "$dir/change-a" & a=$!
  sleep 1.5
  "$bin" auto --revalidate 2500 < "$dir/short" > "$dir/change-timing" 2>&1 &
  sleep 0.5
  python3 target.py gtk "$dir/change-b" & b=$!
  sleep 3.5
  clipboard=$(timeout 2 wl-paste -n 2>/dev/null || true)
  kill "$a" "$b" 2>/dev/null || true; sleep 0.5
  python3 - "$dir/change-a" "$dir/change-b" "$dir/change-timing" "$clipboard" "$(cat "$dir/short")" <<'PY'
import json, sys
a, b, timing, clipboard, expected = sys.argv[1:]
read = lambda p: open(p).read() if __import__("os").path.exists(p) else ""
t = json.loads(open(timing).read().splitlines()[0])
print(json.dumps({"target": "changed-window", **t, "inserted_a": read(a) != "", "inserted_b": read(b) != "",
                  "clipboard_holds_text": clipboard == expected}), flush=True)
PY
done
