#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if command -v cargo-nextest >/dev/null; then
    bash scripts/test.sh -p sanvi-http -E 'test(quota_) | test(external_quotas)'
    bash scripts/test.sh -p sanvi-api --test integration -E 'test(task026_usage)'
else
    eval "$(bash scripts/test-db.sh up)"
    cargo test -p sanvi-http quota_
    cargo test -p sanvi-http external_quotas
    cargo test -p sanvi-api --test integration task026_usage
fi
