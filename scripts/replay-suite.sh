#!/usr/bin/env bash
# Real assembled-app response capture; existing e2e contract corpus runs too.
set -euo pipefail
cd "$(dirname "$0")/.."
export PYTHONDONTWRITEBYTECODE=1
if [[ "${1:-}" == "--self-test" ]]; then exec python3 scripts/test-api-contract.py; fi
candidate="${1:-local}"
if [[ ! "$candidate" =~ ^[A-Za-z0-9._-]+$ ]]; then echo 'invalid candidate tag' >&2; exit 1; fi
mkdir -p target/replay
export SANVI_REPLAY_CAPTURE="$(pwd)/target/replay/$candidate.json"
rm -f "$SANVI_REPLAY_CAPTURE" "$SANVI_REPLAY_CAPTURE.usage.json"
if command -v cargo-nextest >/dev/null; then
    bash ./scripts/test.sh -p sanvi-api --test integration -E 'test(task026_replay) | test(task026_usage) | test(http_contract) | test(headers_snapshot) | test(cache_headers_snapshot)'
else
    eval "$(bash ./scripts/test-db.sh up)"
    cargo test -p sanvi-api --test integration task026_replay
    cargo test -p sanvi-api --test integration task026_usage
    cargo test -p sanvi-api --test integration http_contract
    cargo test -p sanvi-api --test integration headers_snapshot
    cargo test -p sanvi-api --test integration cache_headers_snapshot
fi
python3 scripts/api-contract.py replay --capture "$SANVI_REPLAY_CAPTURE"
python3 scripts/test-api-contract.py
