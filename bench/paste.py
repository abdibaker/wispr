#!/usr/bin/env python3
"""Check repeated Ctrl+V in a Wayland session. Do not type during the check."""
import json
import pathlib
import subprocess
import sys
import tempfile
import time

bench = pathlib.Path(__file__).resolve().parent
binary = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else (
    bench.parent / "src-tauri/target/debug/examples/inserttest"
)
expected = "Voice Prompt paste check\nsecond line"

with tempfile.TemporaryDirectory(prefix="vp-paste-") as directory:
    for trial in range(2):
        output = pathlib.Path(directory) / str(trial)
        process = subprocess.Popen(
            [sys.executable, str(bench / "target.py"), "gtk", str(output)],
        )
        try:
            time.sleep(3)
            assert process.poll() is None, "GTK target failed to start"
            result = subprocess.run(
                [str(binary), "paste", "--expect-app", "target.py"], input=expected, text=True,
                capture_output=True, check=True, timeout=15,
            )
            delivery = json.loads(result.stdout)
            assert delivery["delivery"] == "pasted", delivery
            actual = output.read_text()
            assert actual == expected, f"Expected {expected!r}, received {actual!r}"
            print(f"PASS Ctrl+V trial {trial + 1}", flush=True)
        finally:
            process.terminate()
            process.wait(timeout=5)
