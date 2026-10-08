#!/usr/bin/env bash
# TASK-023 (FR-1112, NFR-1105) gate: no unbounded outbound call exists.
# Every `reqwest::Client::builder()` chain must set `.timeout(...)` before
# `.build()`. Circuit breakers and timeouts are not flag-gated, so an
# unbounded client is never the safer default — and the build fails on one.
#
# Usage:
#   scripts/check-timeouts.sh              # the gate
#   scripts/check-timeouts.sh --self-test  # plants a timeout-less client
#                                          # in a real src tree and expects
#                                          # the gate to fail naming it
set -euo pipefail

cd "$(dirname "$0")/.."

# One builder chain per `reqwest::Client::builder()`: from that line to the
# next `.build()`, a `.timeout(` must appear. (`.connect_timeout(` alone is
# not enough — it bounds only the handshake, never the response wait.)
scan_file() {
    awk '
        /reqwest::Client::builder\(\)/ {
            if (in_builder) {
                printf "%s:%d: reqwest client never reaches .build()\n", FILENAME, start_line
                fail = 1
            }
            in_builder = 1
            has_timeout = 0
            start_line = NR
            next
        }
        in_builder && /\.timeout\(/ { has_timeout = 1 }
        in_builder && /\.build\(\)/ {
            if (!has_timeout) {
                printf "%s:%d: reqwest client without .timeout()\n", FILENAME, start_line
                fail = 1
            }
            in_builder = 0
        }
        END {
            if (in_builder) {
                printf "%s:%d: reqwest client never reaches .build()\n", FILENAME, start_line
                fail = 1
            }
            exit fail
        }
    ' "$1"
}

run_gate() {
    local failures=0
    while IFS= read -r file; do
        scan_file "$file" || failures=$((failures + 1))
    done < <(grep -rl 'reqwest::Client::builder()' crates apps --include='*.rs' | sort)
    return "$failures"
}

if [[ "${1:-}" == "--self-test" ]]; then
    probe="crates/platform/http/src/zz_timeout_selftest.rs"
    trap 'rm -f "$probe"' EXIT
    cat > "$probe" <<'EOF'
// check-timeouts.sh self-test: a reqwest client with no timeout. The gate
// must fail naming exactly this file.
pub fn probe() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(1))
        .build()
        .expect("builds")
}
EOF
    out="$(run_gate)" || status=$?
    status="${status:-0}"
    if [[ "$status" -eq 0 ]]; then
        echo "check-timeouts self-test failed: the planted timeout-less client was not caught" >&2
        echo "$out" >&2
        exit 1
    fi
    if ! grep -q "zz_timeout_selftest.rs" <<<"$out"; then
        echo "check-timeouts self-test failed: the finding did not name the planted file" >&2
        echo "$out" >&2
        exit 1
    fi
    # A connect-timeout-only client must also fail: it bounds the handshake,
    # never the response wait. (The probe above is exactly that shape, so
    # this naming check already proves it.)
    rm -f "$probe"
    trap - EXIT
    # The committed tree must pass.
    if ! out="$(run_gate)"; then
        echo "check-timeouts self-test failed: the committed tree does not pass its own gate" >&2
        echo "$out" >&2
        exit 1
    fi
    echo "check-timeouts self-test: OK (planted timeout-less client caught and named; committed tree clean)"
    exit 0
fi

if ! out="$(run_gate)"; then
    echo "$out" >&2
    echo "check-timeouts: FAILED — every reqwest client needs .timeout() before .build()" >&2
    exit 1
fi
echo "check-timeouts: OK (every reqwest client sets a timeout)"
