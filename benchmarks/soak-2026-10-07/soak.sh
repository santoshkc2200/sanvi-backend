#!/usr/bin/env bash
# TASK-021 11.2i step 8: 24-hour soak — five k6 shapes at the offered rates
# (storefront_read 50 rps, admin_write 20, checkout_create 10, webhook_flood
# 100, conversion_upload 50) for PERF_DURATION (default 86400s = 24 h), with
# the resource sampler (benchmarks/soak-2026-10-07/sampler.sh) running
# alongside: RSS, thread/handle counts, per-process CPU, api pool gauges,
# pg_stat_activity connections.
#
# Exits when k6 finishes; the sampler stops itself on the same condition.
# Then run summarize.sh against samples.txt.
set -uo pipefail
cd "$(dirname "$0")/../.."

export SANVI_ENV=local
export SANVI__DATABASE__NAME=sanvi_scale
export SANVI__DATABASE__PORT=5433
export SANVI__DATABASE__PASSWORD=app-local-dev
export SANVI__SERVER__RATE_LIMIT__ENABLED=false
export SANVI__SERVER__TENANT__TENANT_RATE_LIMIT__ENABLED=false
export SANVI__FEATURES__ADVERTISING=true
export SANVI__ADVERTISING__TRACKING_SITE_KEY_SIGNING_SECRET=lab-dev-tracking-site-key-signing-secret-0123456789
export SANVI__ADVERTISING__OAUTH_STATE_SIGNING_SECRET=lab-dev-oauth-state-signing-secret-0123456789
# base64 of the 32-byte lab token KEK (dev only)
export SANVI__ADVERTISING__TOKEN_KEK=bGFiLWRldi10b2tlbi1rZWstMDEyMzQ1Njc4OWFiY2Q=
export SANVI__SERVER__ABUSE__TRACKING__PER_IP__LIMIT=10000000
export SANVI__SERVER__ABUSE__TRACKING__PER_ORIGIN__LIMIT=10000000

export PERF_BASE_URL="${PERF_BASE_URL:-http://localhost:8081}"
export PERF_SEED=1
export PERF_TAG="${PERF_TAG:-soak24}"
export PERF_DURATION="${PERF_DURATION:-86400s}"
export PERF_MACHINE='{"cores":20,"mem_gb":16,"os":"Windows 11 26200","arch":"x86_64"}'

soak_dir="benchmarks/soak-2026-10-07"
samples="$soak_dir/samples-${PERF_TAG}.txt"
duration_secs="${PERF_DURATION%s}"
mkdir -p "$soak_dir"

# The sampler is launched first with stop_on_k6_exits=yes and a ceiling one
# sampling interval past the soak duration, so it stops whether k6 finishes
# or dies.
nohup bash "$soak_dir/sampler.sh" 30 "$samples" $((duration_secs + 60)) yes \
    >> "$soak_dir/sampler.log" 2>&1 &
disown

# The k6 processes and the sampler are disowned on purpose: this script
# returns after launching and the run continues under nohup. The artifacts
# are the five benchmarks/<scenario>/soak24.json summaries (written by k6 at
# the end) and samples-soak24.txt (written as it goes).
declare -A rates=([storefront_read]=50 [admin_write]=20 [checkout_create]=10 [webhook_flood]=100 [conversion_upload]=50)
for s in storefront_read admin_write checkout_create webhook_flood conversion_upload; do
    nohup k6 run -q -e PERF_SCENARIO=$s -e PERF_RATE=${rates[$s]} "tests/load/$s.js" \
        > "$soak_dir/k6-$s.log" 2>&1 &
    disown
done
echo "soak: launched at $(date +%H:%M:%S), duration ${PERF_DURATION}, samples in $samples"
exit 0
