#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


COMMANDS = [
    "python3 scripts/test_verify.py",
    "cargo fmt --manifest-path src-tauri/Cargo.toml --check",
    "cargo clippy --locked --offline --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings",
    "cargo test --locked --offline --manifest-path src-tauri/Cargo.toml",
    "python3 bench/evaluate.py",
    "pnpm build",
]


class VerifyTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "project with spaces"
        scripts = self.root / "scripts"
        scripts.mkdir(parents=True)
        self.script = scripts / "verify.sh"
        shutil.copyfile(Path(__file__).with_name("verify.sh"), self.script)
        tools = self.root / "tools"
        tools.mkdir()
        self.trace = self.root / "trace"
        self.environment = {
            **os.environ,
            "PATH": f"{tools}:{os.environ['PATH']}",
            "TRACE": str(self.trace),
            "EXPECTED_ROOT": str(self.root),
            "FAIL_COMMAND": "",
        }
        for name in ("cargo", "python3", "pnpm"):
            tool = tools / name
            tool.write_text(
                '#!/bin/sh\n'
                '[ "$PWD" = "$EXPECTED_ROOT" ] || exit 98\n'
                'command="${0##*/} $*"\n'
                'printf "%s\\n" "$command" >> "$TRACE"\n'
                '[ "$command" != "$FAIL_COMMAND" ] || exit 23\n'
                'exit 0\n'
            )
            tool.chmod(0o755)

    def run_verify(self):
        return subprocess.run(
            ["/bin/bash", str(self.script)],
            cwd=self.temporary.name,
            env=self.environment,
            capture_output=True,
            text=True,
            timeout=10,
        )

    def test_success_runs_every_stage_from_project_root(self):
        result = self.run_verify()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(), COMMANDS)
        self.assertEqual(result.stdout.count("PASS "), len(COMMANDS))
        self.assertIn("Verification passed (", result.stdout)

    def test_failure_stops_at_each_stage_and_preserves_exit_code(self):
        for index, command in enumerate(COMMANDS):
            with self.subTest(command=command):
                self.trace.unlink(missing_ok=True)
                self.environment["FAIL_COMMAND"] = command
                result = self.run_verify()
                self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
                self.assertEqual(self.trace.read_text().splitlines(), COMMANDS[:index + 1])
                self.assertIn("FAIL ", result.stderr)
                self.assertRegex(result.stderr, r"\d+s total\)")
                self.assertNotIn("Verification passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
