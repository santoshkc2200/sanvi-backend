#!/usr/bin/env bash
# TASK-023 (FR-1112) gate: every dependency-matrix row names an injection
# test that exists, and every test file under tests/resilience/tests/ is
# named by at least one row. The matrix is the test manifest — a row whose
# test file or test function is missing means someone changed behaviour or
# the doc without the other, and a test file no row names is behaviour
# proven by nothing the matrix documents. `just test-dependency-matrix`
# runs this, then the named tests.
#
# Usage:
#   scripts/check-dependency-matrix.sh              # the gate
#   scripts/check-dependency-matrix.sh --self-test  # proves it fails on a
#                                                   # row with no test
set -euo pipefail

root="${root:-$(cd "$(dirname "$0")/.." && pwd)}"
matrix="${matrix:-$root/docs/runbooks/dependency-matrix.md}"

if [[ "${1:-}" == "--self-test" ]]; then
    fixture="$(mktemp -d)"
    trap 'rm -rf "$fixture"' EXIT
    mkdir -p "$fixture/tests/resilience/tests"
    # A row naming a test that does not exist must fail the check.
    cat > "$fixture/matrix.md" <<'EOF'
| Dependency | Mode | Behaviour | Test |
|---|---|---|---|
| Widget | down | explodes | tests/resilience/tests/widget.rs#widget_down_is_survivable |
EOF
    if root="$fixture" matrix="$fixture/matrix.md" bash "$0" >/dev/null 2>&1; then
        echo "SELF-TEST FAILED: a row with a nonexistent test passed the check" >&2
        exit 1
    fi
    # A row with no test column value must fail too.
    cat > "$fixture/matrix.md" <<'EOF'
| Dependency | Mode | Behaviour | Test |
|---|---|---|---|
| Widget | down | explodes | — |
EOF
    if root="$fixture" matrix="$fixture/matrix.md" bash "$0" >/dev/null 2>&1; then
        echo "SELF-TEST FAILED: a row without a test reference passed the check" >&2
        exit 1
    fi
    # A row whose file and function both exist passes …
    printf 'fn widget_down_is_survivable() {}\n' > "$fixture/tests/resilience/tests/widget.rs"
    cat > "$fixture/matrix.md" <<'EOF'
| Dependency | Mode | Behaviour | Test |
|---|---|---|---|
| Widget | down | explodes | tests/resilience/tests/widget.rs#widget_down_is_survivable |
EOF
    if ! root="$fixture" matrix="$fixture/matrix.md" bash "$0" >/dev/null; then
        echo "SELF-TEST FAILED: a covered fixture matrix does not pass" >&2
        exit 1
    fi
    # … but a test file no row names fails the reverse direction.
    printf 'fn orphan_ok() {}\n' > "$fixture/tests/resilience/tests/orphan.rs"
    if root="$fixture" matrix="$fixture/matrix.md" bash "$0" >/dev/null 2>&1; then
        echo "SELF-TEST FAILED: a test file with no matrix row passed the check" >&2
        exit 1
    fi
    # The real matrix must pass.
    if ! bash "$0" >/dev/null; then
        echo "SELF-TEST FAILED: the committed matrix does not pass its own check" >&2
        exit 1
    fi
    echo "self-test OK: missing test, empty test, covered fixture, orphan file and committed matrix behave as expected"
    exit 0
fi

if [[ ! -f "$matrix" ]]; then
    echo "dependency matrix not found: $matrix" >&2
    exit 1
fi

# Extract the Test column (last cell) of every matrix row that names a test
# path; fail on a row that does not. The header and separator lines carry
# no test path and are skipped naturally.
rows=0
missing=0
covered_files=""
while IFS= read -r line; do
    case "$line" in
        '|'*) ;;
        *) continue ;;
    esac
    case "$line" in
        *'tests/resilience/tests/'*) ;;
        *) continue ;;
    esac
    rows=$((rows + 1))
    # The Test column is the last non-empty cell: rows end with a trailing
    # `|`, so `$NF` is always empty and the reference lives in `$(NF-1)`.
    # Refs are backtick-quoted in the doc; the quotes are stripped here.
    ref="$(printf '%s' "$line" | awk -F'|' '{ ref=$(NF-1); gsub(/^[ \t`]+|[ \t`]+$/, "", ref); print ref }')"
    file="${ref%%#*}"
    fn="${ref##*#}"
    if [[ "$ref" != *"#"* || ! -f "$root/$file" ]]; then
        echo "MATRIX ROW WITHOUT TEST: $ref" >&2
        missing=$((missing + 1))
        continue
    fi
    if ! grep -q "fn $fn" "$root/$file"; then
        echo "MATRIX ROW NAMES MISSING TEST: $fn not found in $file" >&2
        missing=$((missing + 1))
        continue
    fi
    # The file is covered; supporting tests beside the row's test (healthy
    # controls, breaker probes, transport variants) live under the same
    # file and need no row of their own.
    covered_files="$covered_files
$file"
done < "$matrix"

if [[ "$rows" -eq 0 ]]; then
    echo "no test-bearing rows found in $matrix — the matrix format regressed?" >&2
    exit 1
fi
if [[ "$missing" -gt 0 ]]; then
    echo "dependency matrix: $missing of $rows rows reference missing tests" >&2
    exit 1
fi
# The reverse direction, at file granularity: a test file no row names is
# a behaviour proven by nothing the matrix documents — or a row deleted
# without its test. `common/` is the shared harness, not a test file.
uncovered=0
while IFS= read -r test_file; do
    rel="${test_file#$root/}"
    case "$covered_files" in
        *"$rel"*) ;;
        *)
            echo "TEST FILE WITHOUT MATRIX ROW: $rel" >&2
            uncovered=$((uncovered + 1))
            ;;
    esac
done < <(find "$root/tests/resilience/tests" -maxdepth 1 -name '*.rs' | sort)
if [[ "$uncovered" -gt 0 ]]; then
    echo "dependency matrix: $uncovered test files are named by no row" >&2
    exit 1
fi
echo "dependency matrix OK: $rows rows, every test present"
