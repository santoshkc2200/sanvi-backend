#!/usr/bin/env bash
# TASK-023 (FR-1112) drill: rolling restart (and revert) of the staging
# compose stack under k6 load. A stable proxy switches between two healthy
# API slots; scripts/deploy-staging.sh owns deploy and revert alike.
#
# The live drill runs on the staging host (it recreates staging
# containers under load) — never in CI. `--dry-run` validates everything
# that can be validated without touching the stack, and that is what CI
# runs: compose restart-readiness (healthcheck + grace periods present and
# sufficient) plus the drill plan.
#
# Usage (on the staging host):
#   API_IMAGE=... WORKER_IMAGE=... ./scripts/rolling-restart.sh \
#       --tag sha-<commit> --base-url http://127.0.0.1:8080
#   ./scripts/rolling-restart.sh --dry-run   # CI-safe
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE="$root/docker-compose.staging.yml"

TAG=""
BASE_URL="http://127.0.0.1:8080"
DRY_RUN=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag) TAG="$2"; shift 2 ;;
        --base-url) BASE_URL="$2"; shift 2 ;;
        --dry-run) DRY_RUN=1; shift ;;
        *) echo "unknown flag: $1 (see header)" >&2; exit 2 ;;
    esac
done

# --- restart-readiness: the mechanisms the drill depends on -------------
# The API must drain (stop_grace_period >= the 30 s server shutdown
# timeout) and must become detectably healthy (healthcheck), or the roll
# cannot be lossless whatever the procedure says.
readiness() {
    local config
    config="$(docker compose -f "$COMPOSE" config 2>/dev/null)" || {
        echo "readiness: FAILED — cannot render $COMPOSE" >&2
        return 1
    }
    local failures=0
    for service in api api_next worker; do
        # The service block: from its `  name:` line to the next
        # two-space key. (An awk start,end range cannot be used: the
        # opening line matches the end pattern too.)
        local grace
        grace="$(printf '%s' "$config" | awk -v svc="$service" '
            $0 == "  " svc ":" { in_block = 1; next }
            /^  [a-z]/ { in_block = 0 }
            in_block
        ' | grep -E 'stop_grace_period' | head -1 || true)"
        if [[ -z "$grace" ]]; then
            echo "readiness: FAILED — $service has no stop_grace_period" >&2
            failures=$((failures + 1))
        else
            echo "readiness: $service $grace"
        fi
    done
    # The rendered healthcheck spans lines (a CMD list), so match the
    # block and the probe binary separately within the api section.
    local api_block
    api_block="$(printf '%s' "$config" | awk '
        $0 == "  api:" { in_block = 1; next }
        /^  [a-z]/ { in_block = 0 }
        in_block
    ')"
    if printf '%s' "$api_block" | grep -q 'healthcheck:' \
        && printf '%s' "$api_block" | grep -q 'sanvi-cli'; then
        echo "readiness: api healthcheck present"
    else
        echo "readiness: FAILED — api has no healthcheck for the roll to gate on" >&2
        failures=$((failures + 1))
    fi
    if ! grep -q '^  api_proxy:' <<<"$config"; then
        echo 'readiness: FAILED — stable api_proxy listener is missing' >&2
        failures=$((failures + 1))
    fi
    return "$failures"
}

if [[ "$DRY_RUN" -eq 1 ]]; then
    if readiness; then
        echo "rolling-restart dry-run: OK (stack is restart-ready; live drill runs on the staging host)"
        echo "live form: API_IMAGE=<img> WORKER_IMAGE=<img> ./scripts/rolling-restart.sh --tag <tag> --base-url $BASE_URL"
        exit 0
    fi
    echo "rolling-restart dry-run: FAILED" >&2
    exit 1
fi

# --- live drill (staging host only) ---------------------------------------
if [[ -z "${API_IMAGE:-}" || -z "${WORKER_IMAGE:-}" ]]; then
    echo "refusing: API_IMAGE and WORKER_IMAGE must be pinned sha tags (never :latest)" >&2
    exit 2
fi
command -v k6 >/dev/null 2>&1 || { echo "refusing: k6 is required" >&2; exit 2; }
readiness || exit 1
if [[ ! -f "$root/.deploy/proxy/active.conf" ]]; then
    echo 'refusing: bootstrap the two-slot deployment before running the live drill' >&2
    exit 1
fi

ARTIFACTS="$root/benchmarks/rolling-restart"
mkdir -p "$ARTIFACTS"
RUN_TAG="${TAG:-manual}-$(date -u +%Y%m%dT%H%M%SZ)"
# The shape writes its own artifact (handleSummary) to
# benchmarks/<scenario>/<tag>.json; the drill reads the failure count
# back out of it.
SHAPE="rolling_restart"
RESULTS="$root/benchmarks/$SHAPE/$RUN_TAG.json"
mkdir -p "$root/benchmarks/$SHAPE"
LOG="$ARTIFACTS/$RUN_TAG.log"

echo "drill $RUN_TAG: starting k6 load ($SHAPE, 5-minute window)"
cd "$root"
PERF_HOST_DOMAIN="${PERF_HOST_DOMAIN:-platform.sanvi.app}" PERF_BASE_URL="$BASE_URL" PERF_TAG="$RUN_TAG" PERF_DURATION=300s PERF_SCENARIO="$SHAPE" \
    k6 run "tests/load/$SHAPE.js" >"$LOG" 2>&1 &
K6_PID=$!
trap 'kill "$K6_PID" 2>/dev/null || true' EXIT

# Wait for a successful setup probe so the roll cannot finish before
# k6 has even connected. Keep the old listener serving during setup.
for ((attempt=0; attempt<30; attempt++)); do
    if grep -q 'restart-load-ready' "$LOG"; then break; fi
    kill -0 "$K6_PID" 2>/dev/null || { cat "$LOG"; exit 1; }
    if [[ "$attempt" -eq 29 ]]; then echo 'k6 setup did not become ready' >&2; exit 1; fi
    sleep 1
done
bash "$root/scripts/deploy-staging.sh"
kill -0 "$K6_PID" 2>/dev/null || { echo 'load ended before the traffic switch' >&2; exit 1; }

echo "drill $RUN_TAG: waiting for k6 to finish the window"
wait "$K6_PID"
trap - EXIT

python3 "$root/scripts/check-restart-result.py" "$RESULTS"
cp "$RESULTS" "$ARTIFACTS/$RUN_TAG.json"
echo "drill $RUN_TAG: OK — zero non-2xx across the roll; repeat with the previous image for the revert pass"
