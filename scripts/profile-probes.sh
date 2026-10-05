#!/usr/bin/env bash
# TASK-021 step 2 (11.2c): capture the per-endpoint before-number artifacts.
#
# Runs tests/load/profile_probe.js once per candidate endpoint at a uniform
# offered rate and duration, writing benchmarks/profiles/<name>/<tag>.json.
# The top-20 selection then ranks the artifacts by (requests × latency) —
# see benchmarks/profiles/README.md for the ranking record.
#
# Environment (same conventions as `just perf`):
#   PERF_TAG       artifact tag (default rc0)
#   PERF_RATE      offered rate per probe (default 20)
#   PERF_DURATION  per-probe duration (default 30s)
#   PERF_BASE_URL  API base (default http://localhost:8081)
#   PERF_PROBES    space-separated subset of probe names (default: all)
#   PERF_STMT_PG   capture per-endpoint statements: a `psql` command prefix
#                  against the API's database, e.g. on the compose stack
#                  `docker compose exec -T postgres psql -U postgres -d sanvi_scale`.
#                  With it set, every probe resets pg_stat_statements before
#                  its run and dumps the top statements by total time to
#                  benchmarks/profiles/<name>/statements-<tag>.txt — the
#                  server-side attribution each profile record carries next
#                  to its latency before-number.
#
# The API must be running against the seed-scale database with rate limiting
# disabled and the tracking abuse quota raised (the lab posture recorded in
# benchmarks/baseline-2026-10-02.md).

set -euo pipefail
cd "$(dirname "$0")/.."

TAG="${PERF_TAG:-rc0}"
RATE="${PERF_RATE:-20}"
DURATION="${PERF_DURATION:-30s}"
MACHINE="${PERF_MACHINE:-}"

if ! command -v k6 >/dev/null 2>&1; then
  echo "k6 is required" >&2
  exit 1
fi

if [[ -z "$MACHINE" ]]; then
  cores="$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 0)"
  mem_gb=0
  if [[ -r /proc/meminfo ]]; then
    mem_gb="$(awk '/MemTotal/ {print int($2/1048576)}' /proc/meminfo)"
  elif command -v sysctl >/dev/null 2>&1; then
    mem_gb=$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 ))
  elif uname -s | grep -qi 'windows\|mingw\|msys'; then
    mem_gb="$(powershell -NoProfile -Command "[math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory/1GB)" 2>/dev/null || echo 0)"
  fi
  os="$(uname -s) $(uname -r)"
  case "$os" in
    *MINGW*|*MSYS*) os="$(powershell -NoProfile -Command "(Get-CimInstance Win32_OperatingSystem).Caption + ' ' + (Get-CimInstance Win32_OperatingSystem).BuildNumber" 2>/dev/null || echo "$os")" ;;
  esac
  arch="$(uname -m)"
  MACHINE="{\"cores\":${cores},\"mem_gb\":${mem_gb},\"os\":\"${os}\",\"arch\":\"${arch}\"}"
fi

# name|method|path|host|body|headers|expect
# host/body/headers may be empty; JSON fields must not contain pipes.
TENANT_HOST="scale-tail-1-003.localhost"
PROBE_LIST=(
  "public_tenant-context|GET|/api/v1/public/tenant-context|${TENANT_HOST}|||200"
  "public_theme|GET|/api/v1/public/theme|${TENANT_HOST}|||200"
  "tenant_context|GET|/api/v1/tenant/context|${TENANT_HOST}|||200"
  "public_track|POST|/api/v1/public/track||{\"event_id\":\"perf-${TAG}-probe\",\"event_name\":\"Purchase\",\"value_minor\":2500,\"currency\":\"JPY\",\"occurred_at\":\"2026-10-05T00:00:00Z\"}|{\"Origin\":\"https://scale-hot-ads.domains.example\",\"X-Site-Key\":\"not-a-real-key\"}|202"
  "public_locales|GET|/api/v1/public/locales||||200"
  "public_messages|GET|/api/v1/public/messages/errors?locale=ja||||200"
  "public_plans|GET|/api/v1/public/plans||||200"
  "public_privacy-notice|GET|/api/v1/public/privacy/notice||||200"
  "public_notice-at-collection|GET|/api/v1/public/privacy/notice-at-collection||||200"
  "public_privacy-metrics|GET|/api/v1/public/privacy/metrics||||200"
  "public_subprocessors|GET|/api/v1/public/subprocessors||||200"
  "public_slug-availability|GET|/api/v1/public/slug-availability?slug=scale-tail-1-003||||200"
  "system_build|GET|/api/v1/system/build||||200"
  "system_version|GET|/api/v1/system/version||||200,404"
  "healthz|GET|/healthz||||200"
  "readyz|GET|/readyz||||200"
  "metrics|GET|/metrics||||200"
  "openapi_json|GET|/openapi.json||||200"
  "tenant_settings_get|GET|/api/v1/tenant/settings|${TENANT_HOST}|||200,401"
  "tenant_settings_put|PUT|/api/v1/tenant/settings|${TENANT_HOST}|{\"settings\":{\"perf.write\":\"1\"}}||200,401"
  "tenant_me|GET|/api/v1/me||||200,401"
  "tenant_members|GET|/api/v1/tenant/members|${TENANT_HOST}|||200,401"
  "tenant_invoices|GET|/api/v1/tenant/billing/invoices|${TENANT_HOST}|||200,401"
  "tenant_campaigns|GET|/api/v1/tenant/ads/campaigns|${TENANT_HOST}|||200,401"
  "tenant_privacy-requests|GET|/api/v1/tenant/privacy/requests|${TENANT_HOST}|||200,401"
  "tenant_domains|GET|/api/v1/tenant/domains|${TENANT_HOST}|||200,401"
  "tenant_payments|GET|/api/v1/tenant/payments|${TENANT_HOST}|||200,401"
  "tenant_roles|GET|/api/v1/tenant/roles|${TENANT_HOST}|||200,401"
  "tenant_theme-draft|GET|/api/v1/tenant/theme/draft|${TENANT_HOST}|||200,401"
  "tenant_checkout-create|POST|/api/v1/tenant/checkout|${TENANT_HOST}|{\"reference\":\"perf-probe\",\"amount_minor\":1000,\"currency\":\"JPY\",\"success_url\":\"https://scale-hot-ads.domains.example/thanks\",\"cancel_url\":\"https://scale-hot-ads.domains.example/cart\"}||200,401"
  "stripe_connect-webhook|POST|/internal/webhooks/stripe/connect||{\"id\":\"evt_perf_probe\",\"type\":\"charge.succeeded\",\"created\":1759650000,\"data\":{\"object\":{\"id\":\"pi_perf_probe\"}}}|{\"Stripe-Signature\":\"t=1759650000,v1=0000000000000000000000000000000000000000000000000000000000000000\"}|200,401"
  "platform_admin-tenants|GET|/api/v1/platform/admin/tenants||||200,401"
)

SELECTED="${PERF_PROBES:-}"
rc=0
for spec in "${PROBE_LIST[@]}"; do
  IFS='|' read -r name method path host body headers expect <<< "$spec"
  if [[ -n "$SELECTED" ]] && ! [[ " $SELECTED " == *" $name "* ]]; then
    continue
  fi
  echo "probe: $name ($method $path)"
  mkdir -p "benchmarks/profiles/$name"
  if [[ -n "$PERF_STMT_PG" ]]; then
    $PERF_STMT_PG -Atc "SELECT pg_stat_statements_reset();" >/dev/null
  fi
  args=(-e PERF_SCENARIO="profiles/$name" -e PERF_PROBE_NAME="$name"
        -e PERF_PROBE_METHOD="$method" -e PERF_PROBE_PATH="$path"
        -e PERF_RATE="$RATE" -e PERF_DURATION="$DURATION"
        -e PERF_MACHINE="$MACHINE" -e PERF_TAG="$TAG"
        -e PERF_PROBE_EXPECT="$expect")
  [[ -n "$host" ]] && args+=(-e PERF_PROBE_HOST="$host")
  [[ -n "$body" ]] && args+=(-e PERF_PROBE_BODY="$body")
  [[ -n "$headers" ]] && args+=(-e PERF_PROBE_HEADERS="$headers")
  if ! k6 run "${args[@]}" tests/load/profile_probe.js >/dev/null 2>"benchmarks/profiles/$name/.k6-stderr.log"; then
    echo "probe: $name FAILED (see benchmarks/profiles/$name/.k6-stderr.log)" >&2
    rc=1
  fi
  if [[ -n "$PERF_STMT_PG" ]]; then
    {
      echo "# pg_stat_statements during the $name probe (reset immediately before; top 15 by total time)"
      echo "# offered rate ${RATE} rps for ${DURATION}; lab numbers, machine profile in ../${name}/${TAG}.json"
      $PERF_STMT_PG -P pager=off -c "SELECT calls AS calls, round(total_exec_time::numeric,3) AS total_ms, round(mean_exec_time::numeric,3) AS mean_ms, left(regexp_replace(query,E'[\\n\\r\\t ]+',' ','g'),140) AS statement FROM pg_stat_statements ORDER BY total_exec_time DESC LIMIT 15;"
    } > "benchmarks/profiles/$name/statements-$TAG.txt" 2>&1
  fi
done
exit "$rc"
