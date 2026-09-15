#!/usr/bin/env bash
# TASK-031 (phase 11.a) gate: every workspace member's tests are run by a CI
# job. The integration job's crate list was maintained by hand, and a
# hand-maintained list is the defect — a crate missing from it is tested by
# nothing, and a job that runs no tests still passes. This check derives both
# sides from the ground truth: members with a tests/ directory from the
# workspace, and the crates each `cargo nextest run` invocation in pr.yml
# actually selects. Two rules fail the PR:
#   1. a member that ships tests/ which no invocation covers;
#   2. a member excluded from a --workspace invocation that no other
#      invocation names — the way a crate with only inline #[cfg(test)] tests
#      (no tests/ directory) silently stops running.
#
# Usage:
#   scripts/check-ci-test-coverage.sh                      # the gate
#   scripts/check-ci-test-coverage.sh --self-test          # proves it fails
#                                                          # on both rules
#   scripts/check-ci-test-coverage.sh --integration-packages
#                                   # prints `-p X -p Y ...` for every member
#                                   # whose tests use the Docker-backed
#                                   # harness (sanvi_testsupport); the
#                                   # integration job derives its crate list
#                                   # from this instead of a hand-written list
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
workflow="$root/.github/workflows/pr.yml"

# Fail closed on a missing tool rather than degrading into a check that
# parses nothing and passes.
for tool in cargo jq rg; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "this check requires $tool; install it before running" >&2
        exit 1
    fi
done

# name<TAB>manifest_path for every workspace member.
workspace_members() {
    cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" \
        | jq -r '.packages[] | [.name, .manifest_path] | @tsv'
}

# Member names whose package ships a tests/ directory (tsv on stdin).
members_with_tests() {
    local name manifest
    while IFS=$'\t' read -r name manifest; do
        if [[ -d "$(dirname "$manifest")/tests" ]]; then
            printf '%s\n' "$name"
        fi
    done
    return 0
}

# Members whose tests reference sanvi_testsupport — the Docker-backed harness
# (Postgres/Redis) — i.e. exactly the set the integration job must run. One
# name per line.
docker_dependent_names() {
    local members_tsv name manifest dir
    members_tsv="$(workspace_members)"
    while IFS=$'\t' read -r name manifest; do
        dir="$(dirname "$manifest")/tests"
        if [[ -d "$dir" ]] && rg -q 'sanvi_testsupport' "$dir"; then
            printf '%s\n' "$name"
        fi
    done <<<"$members_tsv"
    return 0
}

docker_dependent_args() {
    local args="" name
    while IFS= read -r name; do
        args+="-p $name "
    done < <(docker_dependent_names)
    if [[ -z "$args" ]]; then
        echo "no member's tests use the Docker-backed harness" >&2
        return 1
    fi
    printf '%s\n' "${args% }"
}

# Parse the `cargo nextest run` invocations in $1 and set globals:
#   NEXT_COVERED  — every crate any invocation selects
#   NEXT_EXPLICIT — every crate explicitly named with -p/--package
#   NEXT_EXCLUDED — every crate --exclude'd from a --workspace invocation
# An invocation whose package list is derived in the workflow from
# `--integration-packages` is expanded with the same list this script
# computes, so the gate and the workflow cannot disagree.
parse_nextest_invocations() {
    local wf="$1" members_tsv="$2"
    local invocations all_names line tok pending name workspace
    invocations="$(rg 'cargo nextest run' "$wf" || true)"
    if [[ -z "$invocations" ]]; then
        echo "no 'cargo nextest run' invocation found in $wf" >&2
        return 2
    fi
    all_names="$(printf '%s\n' "$members_tsv" | cut -f1)"
    NEXT_COVERED=""
    NEXT_EXPLICIT=""
    NEXT_EXCLUDED=""
    local -a explicit excluded
    while IFS= read -r line; do
        if [[ "$line" == *'integration_packages.outputs.args'* ]]; then
            line="$line $(docker_dependent_args)" || return 2
        fi
        explicit=()
        excluded=()
        pending=""
        workspace=0
        for tok in $line; do
            case "$tok" in
                --workspace) workspace=1 ;;
                --exclude=*) excluded+=("${tok#--exclude=}") ;;
                --exclude | --package | -p) pending="$tok" ;;
                --package=*) explicit+=("${tok#--package=}") ;;
                -p*) explicit+=("${tok#-p}") ;;
                *)
                    if [[ -n "$pending" ]]; then
                        if [[ "$pending" == "--exclude" ]]; then
                            excluded+=("$tok")
                        else
                            explicit+=("$tok")
                        fi
                        pending=""
                    fi
                    ;;
            esac
        done
        for name in "${explicit[@]+"${explicit[@]}"}"; do
            NEXT_EXPLICIT+="$name"$'\n'
        done
        for name in "${excluded[@]+"${excluded[@]}"}"; do
            NEXT_EXCLUDED+="$name"$'\n'
        done
        if [[ "$workspace" == 1 ]]; then
            local skip e
            while IFS= read -r name; do
                skip=0
                for e in "${excluded[@]+"${excluded[@]}"}"; do
                    if [[ "$name" == "$e" ]]; then
                        skip=1
                        break
                    fi
                done
                if [[ "$skip" == 0 ]]; then
                    NEXT_COVERED+="$name"$'\n'
                fi
            done <<<"$all_names"
        fi
        for name in "${explicit[@]+"${explicit[@]}"}"; do
            NEXT_COVERED+="$name"$'\n'
        done
    done <<<"$invocations"
    return 0
}

# Print uncovered names prefixed by the rule they broke ("untested" for rule
# 1, "excluded-everywhere" for rule 2); rc 0 = covered, 1 = uncovered, 2 =
# structural (no invocations found).
coverage_report() {
    local wf="$1" members_tsv="$2" with_tests covered uncovered ex
    with_tests="$(printf '%s\n' "$members_tsv" | members_with_tests | sort -u)"
    parse_nextest_invocations "$wf" "$members_tsv" || return 2
    covered="$(printf '%s\n' "$NEXT_COVERED" | sort -u)"
    if [[ -z "$covered" ]]; then
        if [[ -n "$with_tests" ]]; then
            printf 'untested\n%s\n' "$with_tests"
            return 1
        fi
        return 0
    fi
    if [[ -n "$with_tests" ]]; then
        uncovered="$(printf '%s\n' "$with_tests" | grep -FxvF -f <(printf '%s\n' "$covered") || true)"
        if [[ -n "$uncovered" ]]; then
            printf 'untested\n%s\n' "$uncovered"
            return 1
        fi
    fi
    while IFS= read -r ex; do
        [[ -z "$ex" ]] && continue
        if [[ -z "$NEXT_EXPLICIT" ]] || ! grep -Fxq "$ex" <<<"$NEXT_EXPLICIT"; then
            printf 'excluded-everywhere\n%s\n' "$ex"
            return 1
        fi
    done <<<"$NEXT_EXCLUDED"
    return 0
}

if [[ "${1:-check}" == "--self-test" ]]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    members_tsv="$(workspace_members)"
    docker_names="$(docker_dependent_names)"

    if ! coverage_report "$workflow" "$members_tsv" >/dev/null 2>&1; then
        echo "self-test failed: the real workflow was reported as not covering the workspace" >&2
        coverage_report "$workflow" "$members_tsv" >&2 || true
        exit 1
    fi

    # Rule 1: drop a member that ships tests/, is not excluded from the unit
    # job, and does not use the Docker harness, from a temporary workflow
    # copy (it can appear in no -p list by construction, so the derivation
    # cannot re-cover it). The check must fail naming it. When every real
    # member is unit-excluded or Docker-dependent — a shape the repo may
    # legitimately grow into — synthesize the fixture instead: a temp member
    # whose only possible coverage would come from the unit job, so the
    # self-test keeps working when the last qualifying real member is gone.
    victim=""
    while IFS= read -r n; do
        if ! grep -q -- "--exclude $n\$" "$workflow" && ! grep -q -- "--exclude $n " "$workflow" \
            && ! grep -Fxq "$n" <<<"$docker_names"; then
            victim="$n"
            break
        fi
    done <<<"$(printf '%s\n' "$members_tsv" | members_with_tests)"
    fixture_tsv="$members_tsv"
    if [[ -z "$victim" ]]; then
        victim="sanvi-coverage-selftest-fixture"
        mkdir -p "$tmp/$victim/tests"
        : >"$tmp/$victim/Cargo.toml"
        fixture_tsv="$(printf '%s\n%s\t%s\n' "$members_tsv" "$victim" "$tmp/$victim/Cargo.toml")"
    fi
    sed -e "s/--exclude sanvi-persistence/--exclude sanvi-persistence --exclude $victim/" \
        "$workflow" >"$tmp/dropped.yml"
    out="$(coverage_report "$tmp/dropped.yml" "$fixture_tsv" 2>&1 || true)"
    if ! grep -q "^untested$" <<<"$out" || ! grep -Fxq "$victim" <<<"$out"; then
        echo "self-test failed: a dropped crate was not reported as untested" >&2
        printf '%s\n' "$out" >&2
        exit 1
    fi

    # Rule 2: exclude a member with no tests/ directory (so rule 1 cannot
    # fire) from the unit job; nothing else names it, so its inline tests
    # would run nowhere. The check must fail naming it.
    ghost=""
    while IFS= read -r n; do
        if ! grep -q -- "--exclude $n\$" "$workflow" && ! grep -q -- "--exclude $n " "$workflow" \
            && ! grep -Fxq "$n" <<<"$(printf '%s\n' "$members_tsv" | members_with_tests)" \
            && ! grep -Fxq "$n" <<<"$docker_names"; then
            ghost="$n"
            break
        fi
    done <<<"$(printf '%s\n' "$members_tsv" | cut -f1)"
    if [[ -z "$ghost" ]]; then
        echo "self-test failed: no fixture member without tests/ available to exclude" >&2
        exit 1
    fi
    sed -e "s/--exclude sanvi-persistence/--exclude sanvi-persistence --exclude $ghost/" \
        "$workflow" >"$tmp/ghost.yml"
    out="$(coverage_report "$tmp/ghost.yml" "$members_tsv" 2>&1 || true)"
    if ! grep -q "^excluded-everywhere$" <<<"$out" || ! grep -Fxq "$ghost" <<<"$out"; then
        echo "self-test failed: an excluded-everywhere crate was not reported" >&2
        printf '%s\n' "$out" >&2
        exit 1
    fi

    # A workflow that runs no tests must not pass silently either.
    grep -v 'cargo nextest run' "$workflow" >"$tmp/no-tests.yml"
    if coverage_report "$tmp/no-tests.yml" "$members_tsv" >/dev/null 2>&1; then
        echo "self-test failed: a workflow with no nextest invocation was accepted" >&2
        exit 1
    fi

    echo "ci test coverage self-test: OK (fails on dropped crate '$victim', on excluded-everywhere crate '$ghost', and on a nextest-less workflow)"
    exit 0
fi

if [[ "${1:-check}" == "--integration-packages" ]]; then
    docker_dependent_args
    exit 0
fi

if [[ $# -gt 0 ]]; then
    echo "usage: $0 [--self-test | --integration-packages]" >&2
    exit 2
fi

members_tsv="$(workspace_members)"
n_with_tests="$(printf '%s\n' "$members_tsv" | members_with_tests | grep -c . || true)"
rc=0
out="$(coverage_report "$workflow" "$members_tsv")" || rc=$?
if [[ "$rc" == 2 ]]; then
    echo "ci test coverage failed: no cargo nextest invocation in $workflow" >&2
    exit 1
fi
if [[ "$rc" == 1 ]]; then
    reason="${out%%$'\n'*}"
    names="${out#*$'\n'}"
    if [[ "$reason" == "excluded-everywhere" ]]; then
        echo "ci test coverage failed: members excluded from a --workspace job that no other job names with -p (their tests, including inline #[cfg(test)], run nowhere):" >&2
    else
        echo "ci test coverage failed: crates shipping tests/ that no job in $workflow runs:" >&2
    fi
    printf '%s\n' "$names" >&2
    cat >&2 <<'EOF'
Add them to the job that can run what their tests need: the unit job for
plain tests, the integration job (via --integration-packages) for tests on
the Docker-backed harness. Fix the mechanism, not the list: a crate absent
from a hand-written -p list is tested by nothing, and a job that runs no
tests still passes.
EOF
    exit 1
fi
echo "ci test coverage: OK ($n_with_tests members ship tests/; all covered by pr.yml)"
