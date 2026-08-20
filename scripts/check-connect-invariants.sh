#!/usr/bin/env bash
# The phase-09 "never merchant of record" invariant, as a build gate.
#
#   ./scripts/check-connect-invariants.sh             # check the repo
#   ./scripts/check-connect-invariants.sh --self-test # prove it catches violations
#
# Bans the deprecated Stripe v1 Connect patterns that would quietly make
# the platform the merchant of record or reintroduce v1 capability checks
# (docs/phase-09-tenant-payments/implementation-plan.md, "Key decision"):
#
#   1. the v1 account-type selector  — Accounts v2 (POST /v2/core/accounts)
#      replaces it; a quoted `type: standard|express|custom` literal is the
#      fingerprint of v1 account creation;
#   2. charges_enabled / payouts_enabled — deprecated v1 booleans; go-live
#      is decided by configuration.merchant.capabilities.card_payments.status;
#   3. payment_method_types            — hardcoding methods locks out the
#      local methods (e.g. Japanese buyers) dynamic payment methods exist
#      to surface.
#
# The patterns are deliberately QUOTED-usage only: string literals in code
# (JSON payloads, serde renames, API params). Prose that mentions the ban
# in a comment is fine — code that sends the field is not. docs/ is
# excluded because it describes the ban; this script enforces it.

set -euo pipefail

cd "$(dirname "$0")/.."

# Fingerprint patterns (quoted usage — see header). The account-type
# pattern allows a JSON-quoted key ("type": "express") and a bare one
# (type = 'express'); both are v1 account creation.
PATTERNS=(
    "[\"']?type[\"']?[[:space:]]*[:=][[:space:]]*[\"'](standard|express|custom)[\"']"
    "[\"']charges_enabled[\"']"
    "[\"']payouts_enabled[\"']"
    "[\"']payment_method_types[\"']"
)

# Source files the gate reads; build output, dependencies, the SQLx query
# cache and docs (which documents the ban rather than violating it) are
# never scanned.
scan_files() {
    find . \( -name .git -o -name target -o -name node_modules -o -name .sqlx -o -name docs \) \
        -prune -o -type f -print 2>/dev/null \
        | grep -E '\.(rs|ts|svelte|js|mjs|cjs|toml|yaml|yml|json|sql)$' \
        | grep -v '^./scripts/check-connect-invariants.sh$' \
        || true
}

check() {
    local violations=0
    while IFS= read -r file; do
        for pattern in "${PATTERNS[@]}"; do
            if grep -nE "$pattern" "$file" >/dev/null 2>&1; then
                echo "CONNECT-INVARIANT VIOLATION: $file" >&2
                grep -nE "$pattern" "$file" >&2
                violations=$((violations + 1))
            fi
        done
    done < <(scan_files)
    return "$violations"
}

if [[ "${1:-}" == "--self-test" ]]; then
    # The fixture lives under target/ (pruned from the real scan) and is
    # fed to check() by path via a temporary override of scan_files.
    FIXTURE="target/connect_invariant_fixture.rs"
    mkdir -p target
    trap 'rm -f "$FIXTURE"' EXIT

    scan_files() { printf '%s\n' "$FIXTURE"; }

    # Each violating fixture must make check() FAIL (detection works);
    # the clean fixture must make it PASS (no false positives).

    # v1 account type selector — JSON-quoted key.
    echo 'let body = json!({ "type": "express" });' > "$FIXTURE"
    if check; then
        echo "SELF-TEST FAILED: v1 account-type literal (JSON form) was not detected" >&2
        exit 1
    fi
    echo "self-test OK: v1 account-type literal (JSON form) detected"

    # v1 account type selector — bare key.
    echo "let body = r#\"type = 'custom'\"#;" > "$FIXTURE"
    if check; then
        echo "SELF-TEST FAILED: v1 account-type literal (bare form) was not detected" >&2
        exit 1
    fi
    echo "self-test OK: v1 account-type literal (bare form) detected"

    # Deprecated capability booleans.
    echo 'let go_live = row["charges_enabled"];' > "$FIXTURE"
    if check; then
        echo "SELF-TEST FAILED: charges_enabled literal was not detected" >&2
        exit 1
    fi
    echo "self-test OK: charges_enabled literal detected"

    # Hardcoded payment method types.
    printf 'let params = [("payment_method_types", "card")];\n' > "$FIXTURE"
    if check; then
        echo "SELF-TEST FAILED: payment_method_types literal was not detected" >&2
        exit 1
    fi
    echo "self-test OK: payment_method_types literal detected"

    # Prose mentions (unquoted, in comments) must NOT trip the gate.
    printf '// Never use charges_enabled or payment_method_types; see the plan.\n' > "$FIXTURE"
    if ! check; then
        echo "SELF-TEST FAILED: a prose mention was flagged as a violation" >&2
        exit 1
    fi
    echo "self-test OK: prose mention ignored"
    exit 0
fi

if ! check; then
    echo >&2
    echo "Phase-09 invariant violated: Sanvi is never the merchant of record." >&2
    echo "Accounts v2 + direct charges only; see docs/phase-09-tenant-payments/." >&2
    exit 1
fi
echo "connect invariants: OK"
