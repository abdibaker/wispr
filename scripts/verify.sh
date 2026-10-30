#!/bin/bash
set -eu

cd "$(dirname "${BASH_SOURCE[0]}")/.."

run() {
    local label="$1"
    shift
    local started=$SECONDS
    local status
    printf '\n==> %s\n' "$label"
    if "$@"; then
        printf 'PASS %s (%ss)\n' "$label" "$((SECONDS - started))"
    else
        status=$?
        printf 'FAIL %s (%ss, exit %s; %ss total)\n' "$label" "$((SECONDS - started))" "$status" "$SECONDS" >&2
        exit "$status"
    fi
}

run "Verifier self-check" python3 scripts/test_verify.py
run "Rust formatting" cargo fmt --manifest-path src-tauri/Cargo.toml --check
run "Rust lint" cargo clippy --locked --offline --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
run "Rust tests" cargo test --locked --offline --manifest-path src-tauri/Cargo.toml
run "Evaluator self-check" python3 bench/evaluate.py
run "Frontend typecheck and build" pnpm build

printf '\nVerification passed (%ss total).\n' "$SECONDS"
