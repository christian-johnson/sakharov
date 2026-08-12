#!/usr/bin/env bash
#
# The checks CI runs, in one place.
#
# `.github/workflows/ci.yml` and the pre-commit hook (`.githooks/pre-commit`)
# both call this script, so "it passed locally" and "it passed in CI" cannot
# mean two different sets of checks. The toolchain is pinned in
# rust-toolchain.toml so they also run the same compiler.
#
#   ./scripts/check.sh          clippy + tests (what the hook runs)
#   ./scripts/check.sh --full   ... plus the release build (what CI runs)

set -euo pipefail
cd "$(dirname "$0")/.."

run() {
    printf '\033[1;34m==>\033[0m %s\n' "$*"
    "$@"
}

# Warnings are errors: the codebase is clippy-clean and stays so.
run cargo clippy --all-targets -- -D warnings
run cargo test

if [ "${1:-}" = "--full" ]; then
    run cargo build --release
fi

printf '\033[1;32mall checks passed\033[0m\n'
