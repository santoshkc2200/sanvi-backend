#!/usr/bin/env bash
# TASK-031 (phase 11.a) gate: every workspace member that ships tests is run
# by a CI job. The integration job's crate list was maintained by hand, and a
# hand-maintained list is the defect — a crate missing from it is tested by
# nothing, and a job that runs no tests still passes. This check derives both
# sides from the ground truth: members with a tests/ directory from the
# workspace, and the crates each `cargo nextest run` invocation in pr.yml
# actually selects. It exits non-zero naming any crate in the first set that
# no invocation covers.
#
# Usage:
#   scripts/check-ci-test-coverage.sh                      # the gate
#   scripts/check-ci-test-coverage.sh --self-test          # proves it fails
#                                                          # on a dropped crate
#   scripts/check-ci-test-coverage.sh --integration-packages
#                                   # prints `-p X -p Y ...` for every member
#                                   # whose tests use the Docker-backed
#                                   # harness (sanvi_testsupport); the
#                                   # integration job derives its crate list
#                                   # from this instead of a hand-written list
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
workflow="$root/.github/workflows/pr.yml"

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

# `-p X -p Y ...` for every member whose tests reference sanvi_testsupport —
# the Docker-backed harness — i.e. exactly the set the integration job needs.
docker_dependent_args() {
    local members_tsv name manifest dir args=""
    members_tsv="$(workspace_members)"
    while IFS=$'\t' read -r name manifest; do
        dir="$(dirname "$manifest")/tests"
        if [[ -d "$dir" ]] && rg -q 'sanvi_testsupport' "$dir"; then
            args+="-p $name "
        fi
    done <<<"$members_tsv"
    if [[ -z "$args" ]]; then
        echo "no member's tests use the Docker-backed harness" >&2
        return 1
    fi
    printf '%s\n' "${args% }"
}

# Emit every crate the `cargo nextest run` invocations in $1 select. `--workspace`
# selects all members minus each `--exclude`; `-p`/`--package` select explicitly.
# An invocation whose package list is derived in the workflow from
# `--integration-packages` is expanded with the same list this script computes,
# so the gate and the workflow cannot disagree.
covered_crates() {
    local wf="$1" members_tsv="$2"
    local invocations all_names line tok pending name workspace
    invocations="$(rg 'cargo nextest run' "$wf" || true)"
    if [[ -z "$invocations" ]]; then
        echo "no 'cargo nextest run' invocation found in $wf" >&2
        return 2
    fi
    all_names="$(printf '%s\n' "$members_tsv" | cut -f1)"
    local -a explicit excluded
    while IFS= read -r line; do
        if [[ "$line" == *'integration_packages.outputs.args'* ]]; then
            line="$line $(docker_dependent_args)"
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
                    printf '%s\n' "$name"
                fi
            done <<<"$all_names"
        fi
        for name in "${explicit[@]+"${explicit[@]}"}"; do
            printf '%s\n' "$name"
        done
    done <<<"$invocations"
    return 0
}

# Print the uncovered names; rc 0 = covered, 1 = uncovered, 2 = workflow has no
# nextest invocation at all.
coverage_report() {
    local wf="$1" members_tsv="$2" with_tests covered uncovered
    with_tests="$(printf '%s\n' "$members_tsv" | members_with_tests | sort -u)"
    covered="$(covered_crates "$wf" "$members_tsv" | sort -u)" || return 2
    if [[ -z "$with_tests" ]]; then
        return 0
    fi
    if [[ -z "$covered" ]]; then
        printf '%s\n' "$with_tests"
        return 1
    fi
    uncovered="$(printf '%s\n' "$with_tests" | grep -FxvF -f <(printf '%s\n' "$covered") || true)"
    if [[ -n "$uncovered" ]]; then
        printf '%s\n' "$uncovered"
        return 1
    fi
    return 0
}

if [[ "${1:-check}" == "--self-test" ]]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    members_tsv="$(workspace_members)"

    if ! coverage_report "$workflow" "$members_tsv" >/dev/null 2>&1; then
        echo "self-test failed: the real workflow was reported as not covering the workspace" >&2
        coverage_report "$workflow" "$members_tsv" >&2 || true
        exit 1
    fi

    # Drop the first member that ships tests and is not excluded from the unit
    # job from coverage in a temporary copy: removed from any -p list and added
    # to the unit job's --exclude set. The check must fail naming it.
    with_tests_list="$(printf '%s\n' "$members_tsv" | members_with_tests)"
    victim=""
    while IFS= read -r n; do
        if ! grep -q -- "--exclude $n\$" "$workflow" && ! grep -q -- "--exclude $n " "$workflow"; then
            victim="$n"
            break
        fi
    done <<<"$with_tests_list"
    if [[ -z "$victim" ]]; then
        echo "self-test failed: no crate available to drop from the fixture" >&2
        exit 1
    fi
    sed -e "s/-p $victim / /g" \
        -e "s/--exclude sanvi-persistence/--exclude sanvi-persistence --exclude $victim/" \
        "$workflow" >"$tmp/dropped.yml"
    out="$(coverage_report "$tmp/dropped.yml" "$members_tsv" 2>&1 || true)"
    if ! grep -Fxq "$victim" <<<"$out"; then
        echo "self-test failed: a dropped crate was not reported as uncovered" >&2
        printf '%s\n' "$out" >&2
        exit 1
    fi

    # A workflow that runs no tests must not pass silently either.
    grep -v 'cargo nextest run' "$workflow" >"$tmp/no-tests.yml"
    if coverage_report "$tmp/no-tests.yml" "$members_tsv" >/dev/null 2>&1; then
        echo "self-test failed: a workflow with no nextest invocation was accepted" >&2
        exit 1
    fi

    echo "ci test coverage self-test: OK (fails on dropped crate '$victim' and on a nextest-less workflow)"
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
    echo "ci test coverage failed: crates shipping tests/ that no job in $workflow runs:" >&2
    printf '%s\n' "$out" >&2
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
