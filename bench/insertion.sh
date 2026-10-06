#!/bin/sh
# Paste vs typing in controlled targets: `bench/insertion.sh [trials]`. Needs a Wayland session;
# each target window must take focus when it opens (do not touch the keyboard meanwhile).
set -eu
cd "$(dirname "$0")"
bin=../src-tauri/target/debug/examples/inserttest
trials=${1:-3}
dir=$(mktemp -d)
printf 'fn main() {\n    let path = "src-tauri/Cargo.toml"; // keep — ünïcode ✓\n        indented();\n}\n' > "$dir/code"
python3 -c "print(('Update the retry logic in providers.rs, keep 3 attempts and a 250 ms backoff. ' * 6).strip())" > "$dir/long"
printf 'pnpm check && cargo test --release' > "$dir/short"
for target in gtk vte; do
  for method in type paste paste-terminal; do
    for sample in short code long; do
      for trial in $(seq "$trials"); do
        out="$dir/$target-$method-$sample-$trial"
        python3 target.py "$target" "$out" & pid=$!
        sleep 1.5
        timing=$("$bin" "$method" < "$dir/$sample" 2>&1 || true)
        sleep 1.2
        kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
        python3 - "$target" "$method" "$sample" "$dir/$sample" "$out" "$timing" <<'PY'
import json, sys
target, method, sample, expected_path, out, timing = sys.argv[1:]
expected = open(expected_path).read()
if target == "vte":
    try: got = open(out + ".raw", "rb").read().decode("utf-8", "replace")
    except FileNotFoundError: got = ""
    # Shift+Enter arrives as CR; bracketed paste markers may wrap a paste.
    got = got.replace("\x1b[200~", "").replace("\x1b[201~", "").replace("\r", "\n")
else:
    try: got = open(out).read()
    except FileNotFoundError: got = ""
try: t = json.loads(timing)
except ValueError: t = {"error": timing.strip()[:120]}
print(json.dumps({"target": target, "method": method, "sample": sample, "chars": len(expected),
                  "exact": got == expected, "got_chars": len(got), **t}))
PY
      done
    done
  done
done
rm -rf "$dir"
