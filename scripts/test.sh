#!/usr/bin/env bash
# Run the workspace test suite against the shared test database.
#
# Exists so the suite has an entry point that does not require `just` to be
# installed — the justfile's `test`/`test-quiet` recipes call the same steps.
#
# Usage:
#   scripts/test.sh                 # full output
#   scripts/test.sh --quiet [args]  # failures only (nextest `agent` profile)
#   scripts/test.sh -p sanvi-billing -- --nocapture  # -p/--package scopes to package (omits --workspace)
#   scripts/test.sh --self-test     # unit-test the Gatekeeper detection, run nothing
set -euo pipefail

cd "$(dirname "$0")/.."

# Decision split from detection so it can be unit-tested below: warn when
# `DevToolsSecurity -status` reports developer mode disabled — the state
# `sudo spctl developer-mode enable-terminal` fixes — and stay silent
# otherwise. On a cold run the missing exemption cost ~15 of 16 minutes,
# scanning freshly linked binaries, not testing
# (docs/phase-11-hardening-ga/test-performance-baseline.md).
gatekeeper_notice() {
    case "$1" in
        *"disabled"*)
            cat >&2 <<'EOF'
warning: developer mode is off on this Mac, so Gatekeeper scans every
freshly linked test binary before it can execute — measured at ~15 minutes
per cold run on this workspace, not testing. Before a cold run, exempt the
host:

    sudo spctl developer-mode enable-terminal

then System Settings → Privacy & Security → Developer Tools: enable the
terminal, IDE, or agent host you run the suite from. The exemption is per
parent process, so every new host needs it again.
EOF
            ;;
    esac
}

maybe_warn_gatekeeper() {
    [[ "$(uname -s)" == "Darwin" ]] || return 0
    gatekeeper_notice "$(/usr/sbin/DevToolsSecurity -status 2>/dev/null || true)"
    return 0
}

if [[ "${1:-}" == "--self-test" ]]; then
    out="$(gatekeeper_notice "Developer mode is currently disabled." 2>&1)"
    if [[ -z "$out" ]]; then
        echo "self-test failed: un-exempted host produced no warning" >&2
        exit 1
    fi
    out="$(gatekeeper_notice "Developer mode is currently enabled." 2>&1)"
    if [[ -n "$out" ]]; then
        echo "self-test failed: exempted host produced a warning" >&2
        exit 1
    fi
    out="$(gatekeeper_notice "" 2>&1)"
    if [[ -n "$out" ]]; then
        echo "self-test failed: unavailable DevToolsSecurity produced a warning" >&2
        exit 1
    fi
    echo "gatekeeper detection self-test: OK"
    exit 0
fi

profile_args=()
if [[ "${1:-}" == "--quiet" ]]; then
    shift
    profile_args=(--profile agent)
fi

workspace_args=(--workspace)
for arg in "$@"; do
    if [[ "$arg" == "-p" || "$arg" == "--package" || "$arg" == --package=* || "$arg" == -p?* ]]; then
        workspace_args=()
        break
    fi
done

maybe_warn_gatekeeper

eval "$(./scripts/test-db.sh up)"

if command -v cargo-nextest >/dev/null; then
    exec cargo nextest run "${workspace_args[@]+"${workspace_args[@]}"}" "${profile_args[@]+"${profile_args[@]}"}" "$@"
fi

# nextest is what the profiles are written for; `cargo test` is the fallback
# and has no equivalent of `status-level`, so --quiet is a no-op here.
exec cargo test "${workspace_args[@]+"${workspace_args[@]}"}" "$@"

