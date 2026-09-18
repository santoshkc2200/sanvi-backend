#!/usr/bin/env bash
# TASK-020 gate: the log emitter never renders PII.
#
# Runs the corpus test (crates/platform/telemetry/tests/pii_corpus.rs):
# every PII shape the platform holds goes through the real JSON emitter
# wrapped in Redacted, and the rendered lines are scanned for raw values.
# Zero findings is the only passing result on a clean tree.
#
# `--self-test` proves the scan can fail: it plants a leak (one corpus
# value emitted with its redaction deliberately removed) and asserts the
# scan reports the finding, naming the type.
#
# Usage:
#   scripts/check-log-pii.sh               # gate
#   scripts/check-log-pii.sh --self-test
set -euo pipefail

cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--self-test" ]]; then
    out="$(SANVI_PII_PLANT_LEAK=bearer_token \
        cargo nextest run -p sanvi-telemetry --test pii_corpus \
        -E 'test(planted_leak_is_detected)' 2>&1)" || status=$?
    status="${status:-0}"
    if [[ "$status" -eq 0 ]]; then
        echo "log-pii-scan self-test failed: the planted leak was not caught" >&2
        echo "$out" >&2
        exit 1
    fi
    if ! grep -q "LEAK FINDING: bearer_token" <<<"$out"; then
        echo "log-pii-scan self-test failed: the finding did not name the leaked type" >&2
        echo "$out" >&2
        exit 1
    fi
    echo "log-pii-scan self-test: OK (the planted bearer_token leak was caught and named)"
    exit 0
fi

out="$(cargo nextest run -p sanvi-telemetry --test pii_corpus 2>&1)" || {
    status=$?
    echo "$out" >&2
    echo "log-pii-scan: FAILED — findings above; every sensitive value must be wrapped in Redacted" >&2
    exit "$status"
}
echo "log-pii-scan: OK (zero findings over the PII corpus: emails, phones, addresses, tokens, card metadata, base64-wrapped token, hashed identifier)"
