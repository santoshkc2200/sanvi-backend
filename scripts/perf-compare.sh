#!/usr/bin/env bash
# TASK-019 (FR-1103): diff two tagged perf runs per metric and exit
# non-zero past a per-metric threshold. Artifacts with different machine
# profiles are refused: a number from another host is not a regression.
#
# Usage:
#   scripts/perf-compare.sh <base-tag> <candidate-tag> [scenario ...]
#
# Per-metric thresholds (a candidate is a regression when ...):
#   http_req_duration_{p95,p99,avg}   candidate > base * 1.20   (+20 %)
#   http_req_failed_rate              candidate - base > 0.01  (+1 pp)
#   dropped_iterations_total          candidate > base
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
benchmarks="$root/benchmarks"

LATENCY_TOLERANCE_PCT="${LATENCY_TOLERANCE_PCT:-20}"
LATENCY_NOISE_FLOOR_MS="${LATENCY_NOISE_FLOOR_MS:-0.5}"
FAILED_RATE_TOLERANCE_PP="${FAILED_RATE_TOLERANCE_PP:-1}"

if ! command -v jq >/dev/null 2>&1; then
    echo "perf-compare requires jq; install it before running" >&2
    exit 1
fi

usage() {
    grep '^#' "$0" | sed -n '2,12p'
    exit 2
}

# A scenario's artifact for one tag, or nothing.
artifact() {
    local scenario="$1" tag="$2"
    [[ -f "$benchmarks/$scenario/$tag.json" ]] && { printf '%s\n' "$benchmarks/$scenario/$tag.json"; return 0; }
    return 1
}

# Refuse to compare numbers that did not come from the same machine.
profiles_differ() {
    local base_json="$1" cand_json="$2"
    [[ "$(jq -c '.machine_profile // {}' "$base_json")" == "$(jq -c '.machine_profile // {}' "$cand_json")" ]] \
        && return 1
    return 0
}

# Compare one scenario. Prints the regressions to stdout; rc 1 = regression
# found, 0 = clean.
compare_scenario() {
    local scenario="$1" base_tag="$2" cand_tag="$3" rc=0
    local base cand
    base="$(artifact "$scenario" "$base_tag")" || {
        echo "SKIP  $scenario: no baseline artifact for tag '$base_tag' (run `just perf $base_tag $scenario`)"
        return 0
    }
    cand="$(artifact "$scenario" "$cand_tag")" || {
        echo "FAIL  $scenario: no candidate artifact for tag '$cand_tag'" >&2
        return 1
    }

    if profiles_differ "$base" "$cand"; then
        echo "FAIL  $scenario: machine profiles differ — never compare across hosts" >&2
        echo "  base: $(jq -c '.machine_profile' "$base")" >&2
        echo "  cand: $(jq -c '.machine_profile' "$cand")" >&2
        return 1
    fi

    local label base_val cand_val pct limit
    for label in http_req_duration_p50_ms http_req_duration_p95_ms http_req_duration_p99_ms http_req_duration_avg_ms; do
        base_val="$(jq -r ".metrics.$label // empty" "$base")"
        cand_val="$(jq -r ".metrics.$label // empty" "$cand")"
        [[ -z "$base_val" || -z "$cand_val" ]] && continue
        pct="$(awk -v c="$cand_val" -v b="$base_val" 'BEGIN { if (b <= 0) { print 0 } else { printf "%.1f", (c - b) / b * 100 } }')"
        limit="$(awk -v b="$base_val" -v t="$LATENCY_TOLERANCE_PCT" 'BEGIN { printf "%.3f", b * (1 + t / 100) }')"
        # A relative threshold alone gates jitter: a sub-millisecond metric
        # moves ±30 % between two quiet runs for no reason at all. A
        # regression is a move that is both relatively and absolutely
        # large — the floor (default 0.5 ms) is twice the largest
        # run-to-run jitter two consecutive quiet runs have produced here,
        # and two orders below the smallest budget the plan sets (200 ms).
        if awk -v c="$cand_val" -v l="$limit" 'BEGIN { exit !(c > l) }'; then
            local delta_ms
            delta_ms="$(awk -v c="$cand_val" -v b="$base_val" 'BEGIN { printf "%.3f", c - b }')"
            if awk -v d="$delta_ms" -v f="$LATENCY_NOISE_FLOOR_MS" 'BEGIN { exit !(d > f) }'; then
                echo "FAIL  $scenario/$label: $cand_val ms vs base $base_val ms (+${pct}%, +${delta_ms} ms, tolerance +${LATENCY_TOLERANCE_PCT}% and ${LATENCY_NOISE_FLOOR_MS} ms)"
                rc=1
            fi
        fi
    done

    base_val="$(jq -r '.metrics.http_req_failed_rate // 0' "$base")"
    cand_val="$(jq -r '.metrics.http_req_failed_rate // 0' "$cand")"
    local delta
    delta="$(awk -v c="$cand_val" -v b="$base_val" 'BEGIN { printf "%.4f", (c - b) * 100 }')"
    if awk -v d="$delta" -v t="$FAILED_RATE_TOLERANCE_PP" 'BEGIN { exit !(d > t) }'; then
        echo "FAIL  $scenario/http_req_failed_rate: ${cand_val} vs base ${base_val} (+${delta} pp, tolerance +${FAILED_RATE_TOLERANCE_PP} pp)"
        rc=1
    fi

    base_val="$(jq -r '.metrics.dropped_iterations_total // 0' "$base")"
    cand_val="$(jq -r '.metrics.dropped_iterations_total // 0' "$cand")"
    if awk -v c="$cand_val" -v b="$base_val" 'BEGIN { exit !(c > b) }'; then
        echo "FAIL  $scenario/dropped_iterations: $cand_val dropped vs base $base_val"
        rc=1
    fi

    [[ $rc == 0 ]] && echo "OK    $scenario"
    return "$rc"
}

if [[ "${1:-}" == "--self-test" ]]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    # Point the artifact root at the fixture tree for the whole self-test.
    benchmarks="$tmp/benchmarks"
    scenario="selftest"
    mkdir -p "$benchmarks/$scenario"
    profile='{"cores":10,"mem_gb":32,"os":"selftest","arch":"arm64"}'
    jq -n --argjson p "$profile" '{scenario:"selftest",tag:"base",machine_profile:$p,metrics:{
        http_req_duration_p50_ms:10, http_req_duration_p95_ms:100, http_req_duration_p99_ms:150,
        http_req_duration_avg_ms:20, http_reqs_total:1000, http_req_failed_rate:0,
        iterations_total:1000, dropped_iterations_total:0}}' >"$benchmarks/$scenario/base.json"
    # A run against itself is not a regression.
    cp "$benchmarks/$scenario/base.json" "$benchmarks/$scenario/same.json"
    if ! compare_scenario "$scenario" base same >/dev/null; then
        echo "self-test failed: identical artifacts were reported as a regression" >&2
        exit 1
    fi
    # An injected 20 % latency increase must fail.
    jq '.tag="regressed" | .metrics.http_req_duration_p95_ms *= 1.25 | .metrics.http_req_duration_p99_ms *= 1.25' \
        "$benchmarks/$scenario/base.json" >"$benchmarks/$scenario/regressed.json"
    if compare_scenario "$scenario" base regressed >/dev/null; then
        echo "self-test failed: an injected 25 % latency increase was not caught" >&2
        exit 1
    fi
    # A different machine profile must be refused outright.
    jq '.machine_profile.cores = 4' "$benchmarks/$scenario/base.json" >"$benchmarks/$scenario/otherhost.json"
    if compare_scenario "$scenario" base otherhost >/dev/null 2>&1; then
        echo "self-test failed: a foreign machine profile was not refused" >&2
        exit 1
    fi
    # Sub-noise jitter (+30 % on a 1 ms metric — +0.3 ms, under the floor)
    # is run-to-run noise, not a regression: the fixture that forced this
    # rule into the threshold (webhook_flood, two quiet runs, p50 +28 % on
    # 0.36 ms) must pass.
    jq -n --argjson p "$profile" '{scenario:"selftest",tag:"jitter",machine_profile:$p,metrics:{
        http_req_duration_p50_ms:1.3, http_req_duration_p95_ms:1.0, http_req_duration_p99_ms:1.5,
        http_req_duration_avg_ms:1.1, http_reqs_total:1000, http_req_failed_rate:0,
        iterations_total:1000, dropped_iterations_total:0}}' >"$benchmarks/$scenario/jitter.json"
    jq -n --argjson p "$profile" '{scenario:"selftest",tag:"quiet",machine_profile:$p,metrics:{
        http_req_duration_p50_ms:1.0, http_req_duration_p95_ms:1.0, http_req_duration_p99_ms:1.0,
        http_req_duration_avg_ms:1.0, http_reqs_total:1000, http_req_failed_rate:0,
        iterations_total:1000, dropped_iterations_total:0}}' >"$benchmarks/$scenario/quiet.json"
    if ! compare_scenario "$scenario" quiet jitter >/dev/null; then
        echo "self-test failed: sub-noise jitter (+0.3 ms) was reported as a regression" >&2
        exit 1
    fi
    echo "self-test: OK (identical passes, +25% fails, foreign host refused, sub-noise jitter passes)"
    exit 0
fi

[[ $# -ge 2 ]] || usage
base_tag="$1"
cand_tag="$2"
shift 2
scenarios=("$@")
if [[ ${#scenarios[@]} == 0 ]]; then
    # bash 3.2 (macOS) has no mapfile: fill the array portably.
    while IFS= read -r dir; do
        scenarios+=("$(basename "$dir")")
    done < <(find "$benchmarks" -mindepth 1 -maxdepth 1 -type d | sort)
fi

rc=0
for scenario in "${scenarios[@]}"; do
    compare_scenario "$scenario" "$base_tag" "$cand_tag" || rc=1
done
if [[ $rc != 0 ]]; then
    echo "perf-compare: FAIL — a regression against '$base_tag' blocks the merge (NFR-1101)" >&2
fi
exit "$rc"
