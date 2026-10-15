#!/usr/bin/env bash
# TASK-023 (FR-1112) gate: expand/contract migrations.
#
# A migration touching a live column fails CI unless it ships with a
# matching contract step in the same file or an explicit annotation.
# Rolling deploys run old and new code against one schema: a bare
# `DROP/RENAME COLUMN`, a type change, or a new `NOT NULL` without a
# backfilled default breaks the version still serving traffic. The safe
# shapes are expand (add nullable / add with default), migrate (backfill),
# contract (drop the old) — spread across deploys, or annotated here when
# the table is provably offline.
#
# The gate scans every UP migration (`migrations/*.sql` except `*.down.sql`;
# down migrations undo by design). Pre-gate history that shipped before
# this discipline lives in `scripts/migration-expand-contract-baseline.txt`
# with reasons — old migration files are never edited (sqlx checksums).
#
# An annotation is a comment line in the migration file:
#   -- expand-contract: <why this is safe>
#   -- compat: <why this is safe>
#
# Usage:
#   scripts/check-migration-expand-contract.sh              # the gate
#   scripts/check-migration-expand-contract.sh --self-test  # fixture
#          # migrations proving each rule fails and each safe shape passes,
#          # plus the committed tree passing on its baseline
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
MIGRATIONS="$root/migrations"
BASELINE="$root/scripts/migration-expand-contract-baseline.txt"

scan_tree() {
    local dir="$1" baseline="$2"
    python3 - "$dir" "$baseline" <<'PY'
import os, re, sys

migrations_dir, baseline_file = sys.argv[1], sys.argv[2]

baselined = set()
if os.path.exists(baseline_file):
    for line in open(baseline_file):
        line = line.split('#', 1)[0].strip()
        if line:
            baselined.add(line.split()[0])

ANNOT = re.compile(r'--\s*(expand-contract|compat)\s*:', re.IGNORECASE)

def check_file(path):
    """Return a list of violation strings, or [] when clean."""
    text = open(path).read()
    name = os.path.basename(path)
    annotations = ANNOT.findall(text)
    if annotations:
        return []
    # Strip comment lines so commented-out SQL cannot trip (or satisfy).
    code = re.sub(r'(?m)^\s*--.*$', '', text)
    flat = re.sub(r'\s+', ' ', code)
    upper = flat.upper()
    violations = []

    def flag(rule, detail):
        violations.append(f"{name}: {rule}: {detail}")

    if re.search(r'\bDROP\s+COLUMN\b', upper):
        flag('drop-column', 'DROP COLUMN breaks readers of the old schema')
    if re.search(r'\bDROP\s+TABLE\b', upper):
        flag('drop-table', 'DROP TABLE needs an archival/offboarding note')
    if re.search(r'\bRENAME\s+(COLUMN|TO)\b', upper):
        flag('rename', 'RENAME breaks the old name in one step; add-then-drop instead')
    if re.search(r'\bALTER\s+COLUMN\s+\S+\s+TYPE\b', upper):
        flag('alter-type', 'ALTER COLUMN TYPE rewrites the table under old readers')
    if re.search(r'\bALTER\s+COLUMN\s+\S+\s+DROP\s+NOT\s+NULL\b', upper):
        flag('drop-not-null', 'dropping NOT NULL widens what old writers may store')
    if re.search(r'\bALTER\s+COLUMN\s+\S+\s+SET\s+NOT\s+NULL\b', upper):
        if not re.search(r'\bUPDATE\b', upper):
            flag('set-not-null', 'SET NOT NULL with no backfill UPDATE in the file')
    # ADD COLUMN ... NOT NULL without a DEFAULT blocks on existing rows
    # (and locks the table while it checks them): expand with a default.
    for match in re.finditer(r'\bADD\s+COLUMN\b', upper):
        segment = upper[match.start():match.start() + 500].split(';', 1)[0]
        if re.search(r'\bNOT\s+NULL\b', segment) and 'DEFAULT' not in segment:
            flag('add-not-null-without-default', segment.strip()[:100])
            break
    # A dropped constraint must be replaced in the same step (the swap is
    # the contract); an unmatched drop leaves the column unguarded.
    drops = len(re.findall(r'\bDROP\s+CONSTRAINT\b', upper))
    adds = len(re.findall(r'\bADD\s+(?:CONSTRAINT|PRIMARY\s+KEY|FOREIGN\s+KEY|CHECK|UNIQUE)\b', upper))
    if drops > adds:
        flag('unpaired-drop-constraint', f'{drops} DROP CONSTRAINT vs {adds} ADD in the file')
    return violations

failures = []
scanned = 0
for entry in sorted(os.listdir(migrations_dir)):
    if not entry.endswith('.sql') or entry.endswith('.down.sql'):
        continue
    if entry in baselined:
        continue
    scanned += 1
    failures.extend(check_file(os.path.join(migrations_dir, entry)))

for failure in failures:
    print(failure)
print(f"expand-contract: {len(failures)} violation(s) in {scanned} scanned migration(s)")
sys.exit(1 if failures else 0)
PY
}

if [[ "${1:-}" == "--self-test" ]]; then
    fixture="$(mktemp -d)"
    trap 'rm -rf "$fixture"' EXIT
    mkdir -p "$fixture/migrations"
    touch "$fixture/empty-baseline.txt"

    # Each fixture is one rule's violation.
    cat > "$fixture/migrations/01_rename.sql" <<'EOF'
ALTER TABLE t RENAME COLUMN a TO b;
EOF
    cat > "$fixture/migrations/02_drop.sql" <<'EOF'
ALTER TABLE t DROP COLUMN a;
EOF
    cat > "$fixture/migrations/03_notnull.sql" <<'EOF'
ALTER TABLE t ALTER COLUMN a SET NOT NULL;
EOF
    # ...and each of these must pass: an annotation, a constraint swap,
    # a backfilled NOT NULL, a defaulted ADD.
    cat > "$fixture/migrations/04_annotated.sql" <<'EOF'
-- expand-contract: offline backfill; table is empty until the seed runs.
ALTER TABLE t DROP COLUMN a;
EOF
    cat > "$fixture/migrations/05_swap.sql" <<'EOF'
ALTER TABLE t DROP CONSTRAINT t_check, ADD CONSTRAINT t_check CHECK (a > 0);
EOF
    cat > "$fixture/migrations/06_backfilled.sql" <<'EOF'
UPDATE t SET a = 0 WHERE a IS NULL;
ALTER TABLE t ALTER COLUMN a SET NOT NULL;
EOF
    cat > "$fixture/migrations/07_defaulted.sql" <<'EOF'
ALTER TABLE t ADD COLUMN a bigint NOT NULL DEFAULT 0;
EOF
    expect_fail() {
        if scan_tree "$fixture/migrations" "$fixture/empty-baseline.txt" >/dev/null 2>&1; then
            echo "SELF-TEST FAILED: $1" >&2
            exit 1
        fi
    }
    rm "$fixture/migrations/04_annotated.sql" "$fixture/migrations/05_swap.sql" \
       "$fixture/migrations/06_backfilled.sql" "$fixture/migrations/07_defaulted.sql"
    expect_fail "a bare RENAME/DROP/SET-NOT-NULL passed the gate"
    # NOTE: never `if ! scan | grep` here — with pipefail the pipeline is
    # non-zero whenever the scanner finds violations (its violation exit),
    # whatever grep matched. Capture first, match second.
    findings="$(scan_tree "$fixture/migrations" "$fixture/empty-baseline.txt" 2>&1 || true)"
    if ! grep -q "01_rename.sql.*rename" <<<"$findings"; then
        echo "SELF-TEST FAILED: the finding did not name the rename rule" >&2
        echo "$findings" >&2
        exit 1
    fi
    rm "$fixture/migrations/01_rename.sql" "$fixture/migrations/02_drop.sql" "$fixture/migrations/03_notnull.sql"
    cat > "$fixture/migrations/04_annotated.sql" <<'EOF'
-- expand-contract: offline backfill; table is empty until the seed runs.
ALTER TABLE t DROP COLUMN a;
EOF
    cat > "$fixture/migrations/05_swap.sql" <<'EOF'
ALTER TABLE t DROP CONSTRAINT t_check, ADD CONSTRAINT t_check CHECK (a > 0);
EOF
    cat > "$fixture/migrations/06_backfilled.sql" <<'EOF'
UPDATE t SET a = 0 WHERE a IS NULL;
ALTER TABLE t ALTER COLUMN a SET NOT NULL;
EOF
    cat > "$fixture/migrations/07_defaulted.sql" <<'EOF'
ALTER TABLE t ADD COLUMN a bigint NOT NULL DEFAULT 0;
EOF
    if ! scan_tree "$fixture/migrations" "$fixture/empty-baseline.txt" >/dev/null 2>&1; then
        echo "SELF-TEST FAILED: annotated/swap/backfilled/defaulted shapes did not pass" >&2
        scan_tree "$fixture/migrations" "$fixture/empty-baseline.txt"
        exit 1
    fi
    # The committed tree passes on its baseline — and would fail without
    # it, proving the baseline is load-bearing, not decorative.
    if ! scan_tree "$MIGRATIONS" "$BASELINE" >/dev/null; then
        echo "SELF-TEST FAILED: the committed tree does not pass its own gate" >&2
        scan_tree "$MIGRATIONS" "$BASELINE"
        exit 1
    fi
    if scan_tree "$MIGRATIONS" "$fixture/empty-baseline.txt" >/dev/null 2>&1; then
        echo "SELF-TEST FAILED: the committed tree passes with no baseline" >&2
        exit 1
    fi
    echo "self-test OK: violations fail named, safe shapes pass, committed tree passes on its baseline"
    exit 0
fi

scan_tree "$MIGRATIONS" "$BASELINE"
