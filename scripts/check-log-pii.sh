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

# Bare `cargo nextest` is normally forbidden (CLAUDE.md: the testsupport
# harness strands Postgres containers). The exemption holds only while
# `pii_corpus` stays DB-less — it never touches the Postgres fixture. If a
# test here ever needs the database, route through scripts/test.sh instead.
if [[ "${1:-}" == "--self-test" ]]; then
    # Plant the two easy-to-dismiss shapes by name: a base64-wrapped token
    # and a hashed identifier that is still an identifier. Each planted run
    # must fail the scan and name the leaked type.
    for kind in base64_wrapped_token hashed_identifier; do
        out="$(SANVI_PII_PLANT_LEAK="$kind" \
            cargo nextest run -p sanvi-telemetry --test pii_corpus \
            -E 'test(planted_leak_is_detected)' 2>&1)" || status=$?
        status="${status:-0}"
        if [[ "$status" -eq 0 ]]; then
            echo "log-pii-scan self-test failed: the planted $kind leak was not caught" >&2
            echo "$out" >&2
            exit 1
        fi
        if ! grep -q "LEAK FINDING: $kind" <<<"$out"; then
            echo "log-pii-scan self-test failed: the finding did not name $kind" >&2
            echo "$out" >&2
            exit 1
        fi
    done
    echo "log-pii-scan self-test: OK (planted base64-wrapped-token and hashed-identifier leaks were both caught and named)"
    exit 0
fi

out="$(cargo nextest run -p sanvi-telemetry --test pii_corpus 2>&1)" || {
    status=$?
    echo "$out" >&2
    echo "log-pii-scan: FAILED — findings above; every sensitive value must be wrapped in Redacted" >&2
    exit "$status"
}
echo "log-pii-scan: OK (zero findings over the PII corpus: emails, phones, addresses, tokens, card metadata, base64-wrapped token, hashed identifier)"
