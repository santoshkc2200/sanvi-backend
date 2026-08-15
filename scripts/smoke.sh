#!/usr/bin/env bash
# Smoke test against a running API (default http://localhost:8080).

set -euo pipefail

BASE_URL="${SANVI_BASE_URL:-http://localhost:8080}"

fail() {
    echo "SMOKE FAILED: $1" >&2
    exit 1
}

echo "smoke testing ${BASE_URL}"

curl -fsS "${BASE_URL}/healthz" | grep -q '"ok"' || fail "/healthz"

curl -fsS "${BASE_URL}/readyz" || fail "/readyz"

curl -fsS "${BASE_URL}/api/v1/system/version" | grep -q '"version"' || fail "/api/v1/system/version"

curl -fsS "${BASE_URL}/metrics" | grep -qE '^# (HELP|TYPE)' || fail "/metrics"

curl -fsS "${BASE_URL}/openapi.json" | grep -q '"openapi"' || fail "/openapi.json"

STATUS=$(curl -s -o /dev/null -w '%{http_code}' "${BASE_URL}/healthz" -H 'Origin: http://evil.example')
[[ "${STATUS}" == "200" ]] || fail "unexpected cross-origin status ${STATUS}"

echo "smoke OK"
