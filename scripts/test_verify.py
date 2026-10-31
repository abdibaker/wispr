#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


METADATA = "cargo metadata --locked --offline --manifest-path src-tauri/Cargo.toml --format-version 1"
FMT = "cargo fmt --manifest-path src-tauri/Cargo.toml --check"
CLIPPY = "cargo clippy --locked --offline --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings"
RUST_TESTS = "cargo test --locked --offline --manifest-path src-tauri/Cargo.toml --lib --bins"
GIT = ["git diff --name-only --no-renames HEAD", "git ls-files --others --exclude-standard"]
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
        (self.root / "node_modules").mkdir(parents=True)
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
            "DIFF": "",
            "UNTRACKED": "",
        }
        for name in ("cargo", "python3", "pnpm", "git"):
            tool = tools / name
            tool.write_text(
                '#!/bin/sh\n'
                '[ "$PWD" = "$EXPECTED_ROOT" ] || exit 98\n'
                'command="${0##*/} $*"\n'
                'printf "%s\\n" "$command" >> "$TRACE"\n'
                '[ "$command" != "$FAIL_COMMAND" ] || exit 23\n'
                'case "$command" in\n'
                '  "git diff "*) [ -z "$DIFF" ] || printf "%s\\n" "$DIFF" ;;\n'
                '  "git ls-files "*) [ -z "$UNTRACKED" ] || printf "%s\\n" "$UNTRACKED" ;;\n'
                'esac\n'
                'exit 0\n'
            )
            tool.chmod(0o755)

    def run_verify(self, *arguments):
        return subprocess.run(
            ["/bin/bash", str(self.script), *arguments],
            cwd=self.temporary.name,
            env=self.environment,
            capture_output=True,
            text=True,
            timeout=10,
        )

    def test_success_runs_every_stage_from_project_root(self):
        result = self.run_verify()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(), [METADATA, *COMMANDS])
        self.assertEqual(result.stdout.count("PASS "), len(COMMANDS))
        self.assertIn("Verification passed (", result.stdout)

    def test_failure_stops_at_each_stage_and_preserves_exit_code(self):
        for index, command in enumerate(COMMANDS):
            with self.subTest(command=command):
                self.trace.unlink(missing_ok=True)
                self.environment["FAIL_COMMAND"] = command
                result = self.run_verify()
                self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
                self.assertEqual(self.trace.read_text().splitlines(), [METADATA, *COMMANDS[:index + 1]])
                self.assertIn("FAIL ", result.stderr)
                self.assertRegex(result.stderr, r"\d+s total\)")
                self.assertNotIn("Verification passed", result.stdout)

    def test_missing_dependencies_exit_3_before_any_check(self):
        (self.root / "node_modules").rmdir()
        result = self.run_verify()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("pnpm install --frozen-lockfile", result.stderr)
        self.assertFalse(self.trace.exists())

        (self.root / "node_modules").mkdir()
        self.environment["FAIL_COMMAND"] = METADATA
        result = self.run_verify("changed")
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn("cargo fetch --locked", result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(), [METADATA])

    def test_unknown_mode_is_rejected(self):
        result = self.run_verify("fast")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("usage:", result.stderr)

    def test_changed_runs_only_checks_for_affected_areas(self):
        rust = [FMT, CLIPPY, RUST_TESTS]
        cases = {
            "": [],
            "README.md\ndocs/notes.md\nbench/README.md\nbench/run.py\nbench/cleanup.json\n.gitignore": [],
            "src/Overlay.tsx": ["pnpm build"],
            "index.html\nvite.config.ts": ["pnpm build"],
            "src-tauri/src/streaming.rs": rust,
            "src-tauri/examples/stt.rs": rust,
            "bench/evaluate.py": ["python3 bench/evaluate.py"],
            "scripts/verify.sh": ["python3 scripts/test_verify.py"],
            "src/Settings.tsx\nsrc-tauri/src/settings.rs\nscripts/test_verify.py": [
                "python3 scripts/test_verify.py", *rust, "pnpm build"],
        }
        for diff, expected in cases.items():
            with self.subTest(diff=diff):
                self.trace.unlink(missing_ok=True)
                self.environment["DIFF"] = diff
                result = self.run_verify("changed")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(self.trace.read_text().splitlines(), [METADATA, *GIT, *expected])
                self.assertIn("Changed-file checks passed", result.stdout)
                self.assertNotIn("Verification passed", result.stdout)

    def test_changed_includes_untracked_files(self):
        self.environment["UNTRACKED"] = "src/New.tsx"
        result = self.run_verify("changed")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(), [METADATA, *GIT, "pnpm build"])

    def test_changed_escalates_high_risk_and_unknown_files_to_full_gate(self):
        for path in ("src-tauri/Cargo.toml", "src-tauri/Cargo.lock", "package.json", "pnpm-lock.yaml",
                     "src-tauri/build.rs", "src-tauri/tauri.conf.json", "src-tauri/capabilities/default.json",
                     "src-tauri/protocols/x.xml", "src-tauri/packaging/postinst.sh",
                     ".github/workflows/verify.yml", "new-top-level-file"):
            with self.subTest(path=path):
                self.trace.unlink(missing_ok=True)
                self.environment["DIFF"] = f"src/Overlay.tsx\n{path}"
                result = self.run_verify("changed")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(f"Escalating to the full gate: {path}", result.stdout)
                self.assertEqual(self.trace.read_text().splitlines(), [METADATA, *GIT, *COMMANDS])
                self.assertIn("Verification passed (", result.stdout)

    def test_changed_failure_preserves_exit_code(self):
        self.environment["DIFF"] = "src-tauri/src/pipeline.rs"
        self.environment["FAIL_COMMAND"] = CLIPPY
        result = self.run_verify("changed")
        self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
        self.assertEqual(self.trace.read_text().splitlines(), [METADATA, *GIT, FMT, CLIPPY])
        self.assertNotIn("passed", result.stdout)

    def test_changed_stops_when_git_fails(self):
        self.environment["FAIL_COMMAND"] = GIT[0]
        result = self.run_verify("changed")
        self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
        self.assertNotIn("passed", result.stdout)


if __name__ == "__main__":
    unittest.main()
