#!/usr/bin/env bash
# TASK-024 gate: every accepted advisory finding carries a live expiry.
#
# Acceptances live in deny.toml's [advisories] ignore list as structured
# entries whose reason MUST end with `expires:YYYY-MM-DD`. This check fails
# when an acceptance has no expiry, an unparsable one, or one in the past —
# an acceptance is a loan with a due date, not a pardon.
#
# `--self-test` proves the check can fail: it runs the same logic against
# fixture deny files (past expiry, missing expiry, unparsable expiry — each
# must fail — and a future expiry, which must pass).
#
# Usage:
#   scripts/check-audit-acceptances.sh [--self-test]
#   DENY_TOML=path/to/deny.toml scripts/check-audit-acceptances.sh
set -euo pipefail

cd "$(dirname "$0")/.."

DENY="${DENY_TOML:-deny.toml}"

# Collect expiry findings from [advisories] ignore reasons; fail once at the
# end so every finding is reported in one run.
run_check() {
    local file="$1"
    local findings=()
    [[ -f "$file" ]] || { echo "audit-acceptances: no such file: $file" >&2; return 2; }

    while IFS= read -r line; do
        reason="$(sed -n 's/.*reason *= *"\(.*\)".*/\1/p' <<<"$line")"
        expiry="$(grep -o 'expires:[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}' <<<"$reason" | tail -1 | cut -d: -f2)"
        if [[ -z "$expiry" ]]; then
            findings+=("acceptance without a parsable expiry date: \"$reason\"")
            continue
        fi
        today="$(date +%F)"
        if [[ "$expiry" < "$today" ]]; then
            findings+=("acceptance expired on $expiry: \"$reason\" — re-justify, migrate, or escalate")
        fi
    done < <(awk '/^[[:space:]]*#/ {next} /^\[advisories\]/{in_adv=1;next} /^\[/{in_adv=0} in_adv && /reason *= *"/' "$file")

    if (( ${#findings[@]} )); then
        for f in "${findings[@]}"; do
            echo "AUDIT ACCEPTANCE FINDING: $f" >&2
        done
        return 1
    fi
    return 0
}

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    cat > "$tmp/past.toml" <<'EOF'
[advisories]
ignore = [
  { id = "RUSTSEC-2026-0001", reason = "accepted back then; expires:2020-01-01" },
]
EOF
    cat > "$tmp/missing.toml" <<'EOF'
[advisories]
ignore = [
  { id = "RUSTSEC-2026-0002", reason = "no fixed version yet, will look at it eventually" },
]
EOF
    cat > "$tmp/unparsable.toml" <<'EOF'
[advisories]
ignore = [
  { id = "RUSTSEC-2026-0003", reason = "soon; expires:next tuesday" },
]
EOF
    cat > "$tmp/future.toml" <<'EOF'
[advisories]
ignore = [
  { id = "RUSTSEC-2026-0004", reason = "dev-dependency only; expires:2099-01-01" },
]
EOF
    local ok=1
    for case in past missing unparsable; do
        if DENY_TOML="$tmp/$case.toml" run_check "$tmp/$case.toml" >/dev/null 2>&1; then
            echo "audit-acceptances self-test failed: the $case expiry was not caught" >&2
            ok=0
        fi
    done
    if ! run_check "$tmp/future.toml" >/dev/null 2>&1; then
        echo "audit-acceptances self-test failed: a valid future expiry was rejected" >&2
        ok=0
    fi
    if (( ! ok )); then exit 1; fi
    trap - EXIT
    rm -rf "$tmp"
    echo "audit-acceptances self-test: OK (past, missing and unparsable expiries all caught; future expiry passes)"
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
    exit $?
fi

run_check "$DENY" || {
    echo "audit-acceptances: FAILED — every [advisories] ignore entry needs a live expires:YYYY-MM-DD in its reason (see deny.toml's header)" >&2
    exit 1
}
echo "audit-acceptances: OK (no expired or expiry-less acceptances in $DENY)"
