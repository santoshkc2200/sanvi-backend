#!/usr/bin/env bash
# TASK-020 (FR-1105) gate: the runbook loop is closed in both directions.
#
# - every alert in observability/alerts/rules.json names a runbook that
#   exists and declares the alert in its `**Alerts:**` / `**Alert:**` line;
# - every runbook that declares alerts references only registered ids;
# - every runbook with no alert declaration appears in
#   scripts/runbook-xref-exempt.txt with a reason.
#
# An alert whose runbook is missing, a runbook answering an unregistered
# alert, and an undeclared procedure runbook all fail the gate.
#
# Usage:
#   scripts/check-runbook-xref.sh             # gate
#   scripts/check-runbook-xref.sh --self-test
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"

VIOLATIONS=0
fail() {
    echo "runbook-xref: $1" >&2
    VIOLATIONS=$((VIOLATIONS + 1))
}

# Extract the backticked CamelCase alert ids from a runbook's alert
# declaration, following wrapped lines until the paragraph ends.
declared_alerts() {
    local file="$1"
    awk '
        /^\*\*Alerts?:\*\*/ { collecting = 1 }
        collecting {
            line = $0
            while (match(line, /`[A-Za-z][A-Za-z0-9]*`/)) {
                id = substr(line, RSTART + 1, RLENGTH - 2)
                print id
                line = substr(line, RSTART + RLENGTH)
            }
            if ($0 ~ /[^,]$/) collecting = 0   # paragraph ended
            next
        }
    ' "$file" | sort -u
}

scan_tree() {
    local tree="$1"
    VIOLATIONS=0
    local rules="$tree/observability/alerts/rules.json"
    local exempt="$tree/scripts/runbook-xref-exempt.txt"
    local tsv
    tsv="$(mktemp)"

    # --- alert → runbook ------------------------------------------------
    if [[ ! -f "$rules" ]]; then
        fail "observability/alerts/rules.json is missing"
        return
    fi
    python3 - "$rules" <<'PY' > "$tsv"
import json, sys
doc = json.load(open(sys.argv[1]))
for rule in doc["rules"]:
    print(f'{rule["alert"]}\t{rule["runbook"]}')
PY
    while IFS=$'\t' read -r alert runbook; do
        local path="$tree/$runbook"
        if [[ ! -f "$path" ]]; then
            fail "alert $alert names runbook $runbook which does not exist"
            continue
        fi
        if ! grep -qF "\`$alert\`" "$path"; then
            fail "alert $alert's runbook ($runbook) does not declare it in an **Alerts:** line"
        fi
    done < "$tsv"

    # --- runbook → alert ------------------------------------------------
    for f in "$tree"/docs/runbooks/*.md; do
        [[ -f "$f" ]] || continue
        local rel="docs/runbooks/$(basename "$f")"
        local ids
        ids="$(declared_alerts "$f")"
        if [[ -z "$ids" ]]; then
            if ! awk -v f="$(basename "$f")" '$1 == f { found = 1 } END { exit found ? 0 : 1 }' "$exempt" 2>/dev/null; then
                fail "$rel answers no alert and is not in scripts/runbook-xref-exempt.txt"
            fi
            continue
        fi
        while IFS= read -r id; do
            [[ -z "$id" ]] && continue
            if ! awk -v id="$id" -F'\t' '$1 == id { found = 1 } END { exit found ? 0 : 1 }' "$tsv"; then
                fail "$rel declares alert $id which is not registered in observability/alerts/rules.json"
            fi
        done <<< "$ids"
    done
}

self_test() {
    local tree tmp
    tree="$(mktemp -d)"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tree" "$tmp"' EXIT
    mkdir -p "$tree/observability/alerts" "$tree/docs/runbooks" "$tree/scripts"

    cat > "$tree/observability/alerts/rules.json" <<'EOF'
{
  "rules": [
    {"alert": "GoodAlert", "severity": "page", "symptom": "s", "runbook": "docs/runbooks/good.md",
     "expr": "some_metric_total > 0", "check": {"kind": "increase", "metric": "some_metric_total", "window_secs": 300, "threshold": 0, "for_secs": 0}},
    {"alert": "OrphanAlert", "severity": "page", "symptom": "s", "runbook": "docs/runbooks/missing.md",
     "expr": null, "check": {"kind": "probe", "probe": "SQL: select 1"}},
    {"alert": "UnlistedAlert", "severity": "page", "symptom": "s", "runbook": "docs/runbooks/undeclared.md",
     "expr": null, "check": {"kind": "probe", "probe": "SQL: select 1"}}
  ]
}
EOF
    printf '**Alerts:** `GoodAlert`\n' > "$tree/docs/runbooks/good.md"
    printf 'nothing declared here\n' > "$tree/docs/runbooks/undeclared.md"
    printf '**Alerts:** `GhostAlert`\n' > "$tree/docs/runbooks/phantom.md"
    printf '# exempt on purpose\n' > "$tree/scripts/runbook-xref-exempt.txt"

    scan_tree "$tree" 2> "$tmp/err.md"
    local out
    out="$(cat "$tmp/err.md")"

    grep -q "OrphanAlert names runbook docs/runbooks/missing.md which does not exist" <<<"$out" ||
        { echo "self-test failed: the missing runbook was not caught" >&2; exit 1; }
    grep -q "UnlistedAlert's runbook (docs/runbooks/undeclared.md) does not declare it" <<<"$out" ||
        { echo "self-test failed: the undeclared canonical runbook was not caught" >&2; exit 1; }
    grep -q "docs/runbooks/undeclared.md answers no alert and is not in scripts/runbook-xref-exempt.txt" <<<"$out" ||
        { echo "self-test failed: the alert-less unexempted runbook was not caught" >&2; exit 1; }
    grep -q "docs/runbooks/phantom.md declares alert GhostAlert which is not registered" <<<"$out" ||
        { echo "self-test failed: the unregistered alert reference was not caught" >&2; exit 1; }
    if grep -q "GoodAlert" <<<"$out"; then
        echo "self-test failed: the well-formed pairing was flagged" >&2
        exit 1
    fi

    echo "runbook-xref self-test: OK (missing runbook, undeclared canonical, unexempted procedure file and unregistered reference all fail; the sound pairing passes)"
    exit 0
}

case "${1:-}" in
    --self-test) self_test ;;
esac

scan_tree "$root"

if [[ "$VIOLATIONS" -gt 0 ]]; then
    echo "runbook-xref: FAILED — $VIOLATIONS break(s) in the alert↔runbook loop" >&2
    exit 1
fi
echo "runbook-xref: OK (every alert has its runbook, every runbook declares or is exempted)"
