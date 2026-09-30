#!/usr/bin/env bash
# TASK-024 gate: every secret-shaped struct field is redacted or classified.
#
# Runs the enumeration test (crates/platform/telemetry/tests/
# secret_inventory.rs): the source tree is scanned for secret-shaped struct
# fields; each must be Redacted/SecretRef-wrapped or carry a reasoned,
# rule-satisfying entry in tests/secret-exemptions.txt. The same suite also
# runs the fixture self-test (unclassified field, rule-violating exemption,
# stale entry, clean controls).
#
# `--self-test` additionally plants a brand-new unredacted secret field in a
# real src tree and asserts the gate fails naming it — proving the gate
# catches a *dropped* classification, not just pre-planted fixtures.
#
# Usage:
#   scripts/check-secret-scan.sh               # gate
#   scripts/check-secret-scan.sh --self-test
set -euo pipefail

cd "$(dirname "$0")/.."

# Bare `cargo nextest` is normally forbidden (CLAUDE.md: the testsupport
# harness strands Postgres containers). The exemption holds only while
# `secret_inventory` stays DB-less — it only reads source files. If a test
# here ever needs the database, route through scripts/test.sh instead.
run_gate() {
    cargo nextest run -p sanvi-telemetry --test secret_inventory 2>&1
}

if [[ "${1:-}" == "--self-test" ]]; then
    probe="crates/contexts/domains/src/zz_secret_scan_selftest.rs"
    trap 'rm -f "$probe"' EXIT
    cat > "$probe" <<'EOF'
// check-secret-scan.sh self-test: an unredacted secret-shaped field no
// exemption covers. The gate must fail naming exactly this file and field.
#[derive(Debug, Clone, Serialize)]
pub struct SelfTestProbe {
    pub webhook_secret: String,
}
EOF
    out="$(run_gate)" || status=$?
    status="${status:-0}"
    if [[ "$status" -eq 0 ]]; then
        echo "secret-scan self-test failed: the planted unredacted field was not caught" >&2
        echo "$out" >&2
        exit 1
    fi
    if ! grep -q "zz_secret_scan_selftest.rs:5 SelfTestProbe.webhook_secret" <<<"$out"; then
        echo "secret-scan self-test failed: the finding did not name the planted field" >&2
        echo "$out" >&2
        exit 1
    fi
    rm -f "$probe"
    trap - EXIT
    echo "secret-scan self-test: OK (planted unredacted field caught and named; fixture violations detected)"
    exit 0
fi

out="$(run_gate)" || {
    status=$?
    echo "$out" >&2
    echo "secret-scan: FAILED — findings above; wrap in Redacted<..>/SecretRef or classify in crates/platform/telemetry/tests/secret-exemptions.txt" >&2
    exit "$status"
}
echo "secret-scan: OK (every secret-shaped field is wrapped or carries a rule-satisfying exemption)"
