#!/usr/bin/env bash
# TASK-019 (FR-1103): diff two tagged perf runs per metric and exit
# non-zero past a per-metric threshold. Artifacts with different machine
# profiles are refused: a number from another host is not a regression.
# Neither is a number from a different load shape: duration and offered
# rate must match the baseline. Artifacts also carry a stats_version and
# a build stamp: a missing or mismatched stats_version is refused (the
# percentile definition changed once); so is a missing or failed build
# stamp, and a build-stamp difference is announced — cross-build
# comparison is the gate's job, never a silent one.
#
# Usage:
#   scripts/perf-compare.sh <base-tag> <candidate-tag> [scenario ...]
#
# Per-metric thresholds (a candidate is a regression when ...):
#   http_req_duration_{p50,p95,p99}   candidate > base * 1.20   (+20 %)
#   http_req_failed_rate              candidate - base > 0.01  (+1 pp)
#   dropped_iterations_total          candidate > base
# http_req_duration_avg_ms is recorded in every artifact but never gated:
# on 30–60 s runs its swing on unchanged code (±40 % observed) exceeds
# any useful tolerance, so gating it gates noise.
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
    grep '^#' "$0" | sed -n '2,22p'
    exit 2
}

# A scenario's artifact for one tag, or nothing.
artifact() {
    local scenario="$1" tag="$2"
    [[ -f "$benchmarks/$scenario/$tag.json" ]] && { printf '%s\n' "$benchmarks/$scenario/$tag.json"; return 0; }
    return 1
}

# The scenarios of one comparison: every benchmarks/ directory that holds a
# candidate artifact for the candidate tag. Directory presence alone is not
# the test — since 11.2c/11.2i the tree also carries non-scenario directories
# (profile records, load/spike/soak sample logs), and a missing baseline in
# one of those is not a gate failure, it is not a scenario at all.
default_scenarios() {
    local tag="$1" dir
    while IFS= read -r dir; do
        if [[ -f "$dir/$tag.json" ]]; then
            basename "$dir"
        fi
    done < <(find "$benchmarks" -mindepth 1 -maxdepth 1 -type d | sort)
}

# Refuse to compare numbers that did not come from the same machine.
profiles_differ() {
    local base_json="$1" cand_json="$2"
    [[ "$(jq -c '.machine_profile // {}' "$base_json")" == "$(jq -c '.machine_profile // {}' "$cand_json")" ]] \
        && return 1
    return 0
}

# Refuse to compare runs with different load shapes: duration and offered
# rate define what every number in the artifact means, and the committed
# captures have already mixed 30 s and 60 s runs at the same rate.
shapes_differ() {
    local base_json="$1" cand_json="$2"
    [[ "$(jq -c '[.duration, .offered_rate_rps]' "$base_json")" == "$(jq -c '[.duration, .offered_rate_rps]' "$cand_json")" ]] \
        && return 1
    return 0
}

# Which summary-stat generation an artifact was produced with. 1 = the
# inverted p(0.95)/p(0.99) keys (values are 0.95/0.99 quantiles, below the
# median); 2 = k6 p(95)/p(99). Percentile stats are not comparable across
# generations, so 0 (absent — a pre-versioning or hand-made artifact) is
# refused on both sides.
stats_version() {
    jq -r '.stats_version // 0' "$1"
}

# An artifact whose build stamp is missing, or records a failed probe,
# cannot say what build produced its numbers.
stamp_usable() {
    jq -e '.build != null and (.build | type == "object") and (.build.error == null)' "$1" >/dev/null
}

# Compare one scenario. Prints the regressions to stdout; rc 1 = regression
# found (or no baseline to compare against), 0 = clean.
compare_scenario() {
    local scenario="$1" base_tag="$2" cand_tag="$3" rc=0
    local base cand
    base="$(artifact "$scenario" "$base_tag")" || {
        # A missing baseline must block the merge, not skip: an unbaselined
        # scenario would otherwise never gate anything. (The backticks are
        # escaped — unescaped ones here command-substituted and *ran*
        # `just perf` mid-gate, fabricating the baseline it was missing.)
        echo "FAIL  $scenario: no baseline artifact for tag '$base_tag' (capture one first: \`just perf $base_tag $scenario\`)" >&2
        return 1
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

    if shapes_differ "$base" "$cand"; then
        echo "FAIL  $scenario: load shapes differ — duration and offered rate must match the baseline" >&2
        echo "  base: $(jq -c '{duration, offered_rate_rps}' "$base")" >&2
        echo "  cand: $(jq -c '{duration, offered_rate_rps}' "$cand")" >&2
        return 1
    fi

    local base_stats cand_stats
    base_stats="$(stats_version "$base")"
    cand_stats="$(stats_version "$cand")"
    if [[ "$base_stats" == "0" || "$cand_stats" == "0" ]]; then
        echo "FAIL  $scenario: artifact without stats_version (base '$base_tag': $base_stats, candidate '$cand_tag': $cand_stats) — a capture predating the percentile fix or a hand-made artifact; re-capture it" >&2
        return 1
    fi
    if [[ "$base_stats" != "$cand_stats" ]]; then
        echo "FAIL  $scenario: stats_version differs (base $base_stats vs candidate $cand_stats) — version-1 captures hold 0.95/0.99 quantiles under the p95/p99 names (see the erratum in benchmarks/baseline-2026-09-17.md); re-capture the baseline" >&2
        return 1
    fi

    if ! stamp_usable "$base"; then
        echo "FAIL  $scenario: baseline artifact has no usable build stamp (did GET /api/v1/system/build fail during capture?)" >&2
        return 1
    fi
    if ! stamp_usable "$cand"; then
        echo "FAIL  $scenario: candidate artifact has no usable build stamp (did GET /api/v1/system/build fail during capture?)" >&2
        return 1
    fi
    # -S sorts object keys: two stamps that differ only in serialization
    # order are the same build.
    if [[ "$(jq -cS '.build' "$base")" != "$(jq -cS '.build' "$cand")" ]]; then
        echo "NOTE  $scenario: build stamps differ — any regression found spans two builds"
        echo "  base: $(jq -cS '.build' "$base")"
        echo "  cand: $(jq -cS '.build' "$cand")"
    fi

    local label base_val cand_val pct limit delta
    for label in http_req_duration_p50_ms http_req_duration_p95_ms http_req_duration_p99_ms; do
        base_val="$(jq -r ".metrics.$label // empty" "$base")"
        cand_val="$(jq -r ".metrics.$label // empty" "$cand")"
        # A null (or absent) metric is a failed capture, not a pass: the
        # summary deliberately degrades to nulls when k6 changes shape, so
        # a candidate of all nulls would otherwise gate nothing at all.
        if [[ -z "$base_val" ]]; then
            echo "FAIL  $scenario/$label: baseline artifact has no measurement" >&2
            rc=1
            continue
        fi
        if [[ -z "$cand_val" ]]; then
            echo "FAIL  $scenario/$label: candidate artifact has no measurement" >&2
            rc=1
            continue
        fi
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

    base_val="$(jq -r '.metrics.http_req_failed_rate // empty' "$base")"
    cand_val="$(jq -r '.metrics.http_req_failed_rate // empty' "$cand")"
    if [[ -z "$base_val" || -z "$cand_val" ]]; then
        echo "FAIL  $scenario/http_req_failed_rate: missing measurement" >&2
        rc=1
    else
        delta="$(awk -v c="$cand_val" -v b="$base_val" 'BEGIN { printf "%.4f", (c - b) * 100 }')"
        if awk -v d="$delta" -v t="$FAILED_RATE_TOLERANCE_PP" 'BEGIN { exit !(d > t) }'; then
            echo "FAIL  $scenario/http_req_failed_rate: ${cand_val} vs base ${base_val} (+${delta} pp, tolerance +${FAILED_RATE_TOLERANCE_PP} pp)"
            rc=1
        fi
    fi

    base_val="$(jq -r '.metrics.dropped_iterations_total // empty' "$base")"
    cand_val="$(jq -r '.metrics.dropped_iterations_total // empty' "$cand")"
    if [[ -z "$base_val" || -z "$cand_val" ]]; then
        echo "FAIL  $scenario/dropped_iterations: missing measurement" >&2
        rc=1
    elif awk -v c="$cand_val" -v b="$base_val" 'BEGIN { exit !(c > b) }'; then
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
    build='{"built_at":"now","commit":"abc123","environment":"selftest","version":"1.0.0"}'
    metrics='{"http_req_duration_p50_ms":10, "http_req_duration_p95_ms":100, "http_req_duration_p99_ms":150,
        "http_req_duration_avg_ms":20, "http_reqs_total":1000, "http_req_failed_rate":0,
        "iterations_total":1000, "dropped_iterations_total":0}'

    # One fixture per tag: $1 = tag, $2 = jq filter applied to a fresh
    # stats_version-2 artifact carrying the shared build stamp and the
    # 60 s / 50 rps load shape.
    mk() {
        jq -n --argjson p "$profile" --argjson b "$build" --argjson m "$metrics" \
            '{scenario:"selftest", stats_version:2, build:$b, machine_profile:$p, metrics:$m,
              duration:"60s", offered_rate_rps:50}' \
            | jq "$2" >"$benchmarks/$scenario/$1.json"
    }
    expect_pass() {
        local desc="$1"; shift
        if ! compare_scenario "$@" >/dev/null 2>&1; then
            echo "self-test failed: $desc" >&2
            exit 1
        fi
    }
    expect_fail() {
        local desc="$1"; shift
        if compare_scenario "$@" >/dev/null 2>&1; then
            echo "self-test failed: $desc" >&2
            exit 1
        fi
    }

    mk base '.tag = "base"'
    mk same '.tag = "same"'
    mk regressed '.tag = "regressed" | .metrics.http_req_duration_p95_ms *= 1.25 | .metrics.http_req_duration_p99_ms *= 1.25'
    mk otherhost '.tag = "otherhost" | .machine_profile.cores = 4'
    mk noisyavg '.tag = "noisyavg" | .metrics.http_req_duration_avg_ms = 28'
    mk crossbuild '.tag = "crossbuild" | .build.commit = "def456"'
    mk nobuild '.tag = "nobuild" | .build = null'
    mk badprobe '.tag = "badprobe" | .build = {"error": "build probe failed: 503"}'
    mk nullmetrics '.tag = "nullmetrics" | .metrics.http_req_duration_p50_ms = null | .metrics.http_req_duration_p95_ms = null | .metrics.http_req_duration_p99_ms = null'
    mk stalestats '.tag = "stalestats" | .stats_version = 1 | .stats_note = "inverted-percentile capture"'
    mk nostats '.tag = "nostats" | del(.stats_version)'
    mk shortdur '.tag = "shortdur" | .duration = "30s"'
    mk lowrate '.tag = "lowrate" | .offered_rate_rps = 25'

    # A run against itself is not a regression.
    expect_pass "identical artifacts were reported as a regression" "$scenario" base same
    # An injected latency increase must fail.
    expect_fail "an injected 25 % latency increase was not caught" "$scenario" base regressed
    # A different machine profile must be refused outright.
    expect_fail "a foreign machine profile was not refused" "$scenario" base otherhost
    # So must a different load shape: duration and offered rate define
    # what every number means (the committed captures once mixed 30 s and
    # 60 s runs and compared clean).
    expect_fail "a 30 s candidate against a 60 s baseline was not refused" "$scenario" base shortdur
    expect_fail "a candidate captured at another offered rate was not refused" "$scenario" base lowrate
    # A missing baseline must fail the gate: an unbaselined scenario that
    # skipped would never block a merge — and the backtick bug this case
    # pins down once ran `just perf` here, writing the "missing" baseline
    # from the candidate build before passing.
    expect_fail "a missing baseline artifact was not refused" "$scenario" missingtag base
    # avg is informational: its run-to-run swing on unchanged code (±40 %
    # observed across the committed captures) exceeds any useful
    # tolerance, so a +40 % avg move must not block a merge.
    expect_pass "a +40 % avg swing was reported as a regression" "$scenario" base noisyavg
    # A build-stamp difference is announced, not fatal — cross-build
    # comparison is the gate's job — but never a silent one.
    out="$(compare_scenario "$scenario" base crossbuild)" && crc=0 || crc=$?
    if [[ $crc != 0 ]]; then
        echo "self-test failed: a build-stamp difference was treated as a regression" >&2
        exit 1
    fi
    if ! grep -q "build stamps differ" <<<"$out"; then
        echo "self-test failed: a build-stamp difference was not announced" >&2
        exit 1
    fi
    if grep -q "build stamps differ" <(compare_scenario "$scenario" base same 2>&1); then
        echo "self-test failed: equal builds were announced as differing" >&2
        exit 1
    fi
    # The same stamp serialized with different key order is the same
    # build (the committed artifacts prove the ordering really varies).
    mk keyorder '.tag = "keyorder" | .build = {"version":"1.0.0","environment":"selftest","commit":"abc123","built_at":"now"}'
    if grep -q "build stamps differ" <(compare_scenario "$scenario" base keyorder 2>&1); then
        echo "self-test failed: a build stamp differing only in key order was announced" >&2
        exit 1
    fi
    # An artifact that cannot say what build produced it is refused.
    expect_fail "a null build stamp was not refused" "$scenario" base nobuild
    expect_fail "a failed build probe was not refused" "$scenario" base badprobe
    # Null latency metrics are a failed capture, not a pass: the summary
    # degrades to nulls when k6 changes shape, and an all-null candidate
    # would otherwise gate nothing at all.
    expect_fail "a candidate with null latency metrics was not refused" "$scenario" base nullmetrics
    # Pre-fix captures (stats_version 1, or none) hold 0.95/0.99 quantiles
    # under the p95/p99 names and must never be diffed against version 2.
    expect_fail "a version-1 (inverted) baseline was not refused" "$scenario" stalestats base
    expect_fail "a version-1 (inverted) candidate was not refused" "$scenario" base stalestats
    expect_fail "an artifact without stats_version was not refused" "$scenario" base nostats
    # Sub-noise jitter (+30 % on a 1 ms metric — +0.3 ms, under the floor)
    # is run-to-run noise, not a regression: the fixture that forced this
    # rule into the threshold (webhook_flood, two quiet runs, p50 +28 % on
    # 0.36 ms) must pass.
    mk jitter '.tag = "jitter" | .metrics = {"http_req_duration_p50_ms":1.3, "http_req_duration_p95_ms":1.0, "http_req_duration_p99_ms":1.5, "http_req_duration_avg_ms":1.1, "http_reqs_total":1000, "http_req_failed_rate":0, "iterations_total":1000, "dropped_iterations_total":0}'
    mk quiet '.tag = "quiet" | .metrics = {"http_req_duration_p50_ms":1.0, "http_req_duration_p95_ms":1.0, "http_req_duration_p99_ms":1.0, "http_req_duration_avg_ms":1.0, "http_reqs_total":1000, "http_req_failed_rate":0, "iterations_total":1000, "dropped_iterations_total":0}'
    expect_pass "sub-noise jitter (+0.3 ms) was reported as a regression" "$scenario" quiet jitter

    # The default enumeration scopes itself to directories holding a
    # candidate artifact: the benchmark tree carries non-scenario
    # directories (profile records, soak/sample logs) and their absence of
    # artifacts must not fail the whole comparison.
    mkdir -p "$benchmarks/records-only"
    enum="$(default_scenarios same)"
    if [[ "$enum" != "selftest" ]]; then
        echo "self-test failed: the default enumeration returned '$enum', not 'selftest' — an artifact-less directory leaked into the scenario set" >&2
        exit 1
    fi
    if [[ -z "$(default_scenarios missingtag)" ]]; then
        : # a tag nothing holds is an empty scenario set — the main loop's empty-set guard fails it
    else
        echo "self-test failed: the default enumeration invented scenarios for a tag with no artifacts" >&2
        exit 1
    fi
    # A directory that does hold the candidate but not the baseline is a
    # real scenario and still fails (the missing-baseline rule above).
    mkdir -p "$benchmarks/unbaselined"
    jq -n --argjson p "$profile" --argjson b "$build" --argjson m "$metrics" \
        '{scenario:"unbaselined", stats_version:2, build:$b, machine_profile:$p, metrics:$m,
          duration:"60s", offered_rate_rps:50}' | jq '.tag = "same"' >"$benchmarks/unbaselined/same.json"
    if ! grep -q "unbaselined" <(default_scenarios same); then
        echo "self-test failed: a candidate-bearing directory was dropped from the scenario set" >&2
        exit 1
    fi

    echo "self-test: OK (identical passes; +25% fails; foreign host, missing baseline, null metrics, unusable stamps and version-1 stats refused; build diff announced; avg swing and sub-noise jitter pass)"
    exit 0
fi

[[ $# -ge 2 ]] || usage
base_tag="$1"
cand_tag="$2"
shift 2
scenarios=("$@")
if [[ ${#scenarios[@]} == 0 ]]; then
    # bash 3.2 (macOS) has no mapfile: fill the array portably.
    while IFS= read -r scenario; do
        scenarios+=("$scenario")
    done < <(default_scenarios "$cand_tag")
fi
# An empty set means there is nothing baselined at all: block the merge
# with the reason, rather than passing — or (bash 3.2 + set -u) dying on
# the unbound "${scenarios[@]}" expansion below.
if [[ ${#scenarios[@]} -eq 0 ]]; then
    echo "perf-compare: FAIL — no candidate artifacts for tag '$cand_tag' under $benchmarks (nothing to gate; capture some with \`just perf <tag>\`)" >&2
    exit 1
fi

rc=0
for scenario in "${scenarios[@]}"; do
    compare_scenario "$scenario" "$base_tag" "$cand_tag" || rc=1
done
if [[ $rc != 0 ]]; then
    echo "perf-compare: FAIL — a regression against '$base_tag' blocks the merge (NFR-1101)" >&2
fi
exit "$rc"
