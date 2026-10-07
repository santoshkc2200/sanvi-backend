#!/usr/bin/env bash
# TASK-021 11.2i step 8: soak verdict. Reads the sampler CSV and the five
# k6 soak artifacts and prints the flatness evidence the step's verify line
# asks for: flat memory (per-process RSS first/max/last + slope), no
# connection growth (pg totals, api pool), no unbounded task accumulation
# (thread/handle counts — the per-task count is not instrumented; the
# OS-level proxies and the standing tokio-console gap are recorded), plus
# the run's request totals.
#
# Usage: summarize.sh [samples_csv]   (default benchmarks/soak-2026-10-07/samples.txt)
set -uo pipefail
cd "$(dirname "$0")/../.."

csv="${1:-benchmarks/soak-2026-10-07/samples.txt}"

awk -F, '
NR==1 { next }
$1 ~ /^[0-9]/ {
    n++
    if (n == 1) { first_ts = $1; a3 = a4 = $3; b3 = b4 = $4 }
    if ($3 > amax) amax = $3; if ($4 > wmax) wmax = $4
    if ($3 < amin || amin == 0) amin = $3; if ($4 < wmin || wmin == 0) wmin = $4
    alast = $3; wlast = $4; last_ts = $1
    if ($16 > pgmax) pgmax = $16
    if ($17 > apimax) apimax = $17
    if ($18 > wkmax) wkmax = $18
    if ($11 - $12 > busy) busy = $11 - $12
    if ($15 > afail) afail = $15
    if ($5 > athr) athr = $5; if ($7 > ahdl) ahdl = $7
}
END {
    printf "samples=%d window=%s..%s\n", n, first_ts, last_ts
    printf "rss_mb: api first=%s max=%s last=%s | worker first=%s max=%s last=%s\n", a3, amax, alast, b3, wmax, wlast
    printf "pg conns: total_max=%s api_max=%s worker_max=%s | pool_busy_max=%s acquire_failures_total=%s\n", pgmax, apimax, wkmax, busy, afail
    printf "api threads_max=%s handles_max=%s\n", athr, ahdl
}' "$csv"

echo
for s in storefront_read admin_write checkout_create webhook_flood conversion_upload; do
    f="benchmarks/$s/soak24.json"
    if [[ -f "$f" ]]; then
        jq -r --arg s "$s" '[$s, "reqs="+(.metrics.http_reqs_total|tostring), "failed="+(.metrics.http_req_failed_rate|tostring), "dropped="+(.metrics.dropped_iterations_total|tostring), "p95="+(.metrics.http_req_duration_p95_ms|tostring)+"ms"]|@tsv' "$f"
    else
        echo "$s: soak24.json missing (run still going or k6 died — see $csv / k6 logs)"
    fi
done
