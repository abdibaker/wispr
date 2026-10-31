#!/bin/bash
# Usage: verify.sh [changed]. Without an argument it runs the full gate.
set -eu

cd "$(dirname "${BASH_SOURCE[0]}")/.."

mode="${1:-full}"
case "$mode" in
    full | changed) ;;
    *)
        printf 'usage: %s [changed]\n' "$0" >&2
        exit 2
        ;;
esac
cargo_args=(--locked --offline --manifest-path src-tauri/Cargo.toml)

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

# Exit 3 on missing dependencies so they are not mistaken for a regression.
missing() {
    printf 'SETUP %s\nRun: %s\n' "$1" "$2" >&2
    exit 3
}
[ -d node_modules ] || missing "frontend dependencies are not installed." "pnpm install --frozen-lockfile"
cargo metadata "${cargo_args[@]}" --format-version 1 >/dev/null 2>&1 ||
    missing "locked Rust crates are not in the local cache." "cargo fetch --locked --manifest-path src-tauri/Cargo.toml"

if [ "$mode" = changed ]; then
    rust=0 web=0 evaluator=0 verifier=0
    # Modified, staged, deleted (both rename sides) and untracked files; unknown paths escalate.
    # Assigned first so a git failure stops the script instead of reading as "no changes".
    files=$(git diff --name-only --no-renames HEAD)
    files+=$'\n'$(git ls-files --others --exclude-standard)
    while IFS= read -r file; do
        case "$file" in
            "" | *.md | docs/* | .gitignore | src-tauri/.gitignore) ;;
            bench/evaluate.py) evaluator=1 ;;
            bench/*) ;; # opt-in live and desktop checks; see bench/README.md
            scripts/*) verifier=1 ;;
            src-tauri/src/* | src-tauri/examples/*) rust=1 ;;
            src/* | public/* | index.html | vite.config.ts | tsconfig.json) web=1 ;;
            *)
                printf 'Escalating to the full gate: %s\n' "$file"
                mode=full
                break
                ;;
        esac
    done <<<"$files"
fi

if [ "$mode" = changed ]; then
    [ "$verifier" = 0 ] || run "Verifier self-check" python3 scripts/test_verify.py
    if [ "$rust" = 1 ]; then
        run "Rust formatting" cargo fmt --manifest-path src-tauri/Cargo.toml --check
        run "Rust lint" cargo clippy "${cargo_args[@]}" --all-targets -- -D warnings
        # Skips linking examples and doc tests; clippy above still type-checks every target.
        run "Rust tests" cargo test "${cargo_args[@]}" --lib --bins
    fi
    [ "$evaluator" = 0 ] || run "Evaluator self-check" python3 bench/evaluate.py
    [ "$web" = 0 ] || run "Frontend typecheck and build" pnpm build
    printf '\nChanged-file checks passed (%ss total). Run pnpm verify before handoff.\n' "$SECONDS"
    exit 0
fi

run "Verifier self-check" python3 scripts/test_verify.py
run "Rust formatting" cargo fmt --manifest-path src-tauri/Cargo.toml --check
run "Rust lint" cargo clippy "${cargo_args[@]}" --all-targets -- -D warnings
run "Rust tests" cargo test "${cargo_args[@]}"
run "Evaluator self-check" python3 bench/evaluate.py
run "Frontend typecheck and build" pnpm build

printf '\nVerification passed (%ss total).\n' "$SECONDS"
