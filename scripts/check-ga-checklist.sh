#!/usr/bin/env bash
# TASK-019 (FR-1104) gate: every acceptance-criterion line in
# docs/release/ga-checklist.md carries a state — automated, manual or
# parked — and manual/parked lines carry a verify note (the procedure, or
# the pointer into docs/release/needs-humans.md). A criterion line with no
# state is a criterion nobody has agreed how to check; fail the build.
#
# Usage:
#   scripts/check-ga-checklist.sh [--self-test]
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
checklist="$root/docs/release/ga-checklist.md"

# One awk pass over the file: criterion bullets must start with a state,
# manual/parked bullets must be followed by a `  - verify: ` note before
# the next bullet or section heading.
awk_check() {
    awk '
        BEGIN { bad = 0; pending = 0; verified = 0; last = "" }
        /^- automated — / { next }
        /^- manual — /    { pending = 1; verified = 0; last = $0; next }
        /^- parked — /    { pending = 1; verified = 0; last = $0; next }
        /^  - verify: /   { verified = 1; next }
        /^- / {
            print "no state (use automated|manual|parked): " $0
            bad = 1
            next
        }
        /^## / {
            if (pending && !verified) {
                print "manual/parked line without a verify note: " last
                bad = 1
            }
            pending = 0; verified = 0
            next
        }
        END {
            if (pending && !verified) {
                print "manual/parked line without a verify note: " last
                bad = 1
            }
            exit bad ? 1 : 0
        }
    ' "$1"
}

if [[ "${1:-}" == "--self-test" ]]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT

    printf -- '- No state here at all.\n' >"$tmp/stateless.md"
    if awk_check "$tmp/stateless.md" >/dev/null 2>&1; then
        echo "self-test failed: a criterion line with no state was accepted" >&2
        exit 1
    fi

    printf -- '- maybe — verified by vibes.\n' >"$tmp/unknown.md"
    if awk_check "$tmp/unknown.md" >/dev/null 2>&1; then
        echo "self-test failed: an unknown state was accepted" >&2
        exit 1
    fi

    printf -- '## 99 · test\n\n- manual — something a human does.\n' >"$tmp/noverify.md"
    if awk_check "$tmp/noverify.md" >/dev/null 2>&1; then
        echo "self-test failed: a manual line without a verify note was accepted" >&2
        exit 1
    fi

    if ! awk_check "$checklist" >/dev/null; then
        echo "self-test failed: the committed checklist does not pass its own gate:" >&2
        awk_check "$checklist" >&2 || true
        exit 1
    fi

    echo "ga-checklist self-test: OK (stateless, unknown-state and verify-less lines fail; the committed checklist passes)"
    exit 0
fi

rc=0
out="$(awk_check "$checklist")" || rc=1
if [[ -n "$out" ]]; then
    printf '%s\n' "$out" >&2
fi
if [[ $rc == 0 ]]; then
    n_auto=$(grep -c '^- automated — ' "$checklist" || true)
    n_man=$(grep -c '^- manual — ' "$checklist" || true)
    n_park=$(grep -c '^- parked — ' "$checklist" || true)
    echo "ga-checklist: OK ($n_auto automated, $n_man manual, $n_park parked; every line carries a state)"
fi
exit "$rc"
