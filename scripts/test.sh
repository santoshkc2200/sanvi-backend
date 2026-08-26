#!/usr/bin/env bash
# Run the workspace test suite against the shared test database.
#
# Exists so the suite has an entry point that does not require `just` to be
# installed — the justfile's `test`/`test-quiet` recipes call the same steps.
#
# Usage:
#   scripts/test.sh                 # full output
#   scripts/test.sh --quiet [args]  # failures only (nextest `agent` profile)
#   scripts/test.sh -p sanvi-billing -- --nocapture
set -euo pipefail

cd "$(dirname "$0")/.."

profile_args=()
if [[ "${1:-}" == "--quiet" ]]; then
    shift
    profile_args=(--profile agent)
fi

eval "$(./scripts/test-db.sh up)"

if command -v cargo-nextest >/dev/null; then
    exec cargo nextest run --workspace "${profile_args[@]}" "$@"
fi

# nextest is what the profiles are written for; `cargo test` is the fallback
# and has no equivalent of `status-level`, so --quiet is a no-op here.
exec cargo test --workspace "$@"
