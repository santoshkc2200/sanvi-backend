#!/usr/bin/env bash
# Resource sampler for the TASK-021 11.2i runs (3x, 10x spike, 24 h soak).
#
# Appends one CSV row per sample:
#   ts,host_cpu,api_rss_mb,worker_rss_mb,api_threads,worker_threads,
#   api_handles,worker_handles,api_cpu,worker_cpu,
#   pool_size,pool_idle,acq_count,acq_sum,acq_fail,
#   pg_total,pg_api,pg_worker,pg_active
#
# CPU columns are percents of the whole machine (cores accounted): host_cpu
# is the host-wide instant from typeperf; api_cpu/worker_cpu are deltas of
# the process TotalProcessorTime over the sample interval.
#
# Usage: sampler.sh <interval_secs> <outfile> [max_duration_secs] [stop_when_k6_exits]
# Stop conditions: max_duration elapsed; or, when stop_when_k6_exits is
# "yes", no k6 process running (only the soak passes it — it launches k6
# before the sampler; the 3x/spike windows end on max_duration alone).
set -uo pipefail
# Not -e: under saturation the /metrics scrape or typeperf can fail or be
# slow; the sampler records what it got and keeps sampling — dying mid-window
# is exactly what happened on the first 10x attempt.

interval="${1:?interval}"
outfile="${2:?outfile}"
max_secs="${3:-0}"
stop_on_k6="${4:-no}"

cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1)

pg() {
    docker exec sanvi-postgres-1 psql -U postgres -d sanvi_scale -Atc \
"SELECT count(*),count(*) FILTER (WHERE application_name='sanvi-api'),count(*) FILTER (WHERE application_name='sanvi-worker'),count(*) FILTER (WHERE state='active') FROM pg_stat_activity" 2>/dev/null | tr '|' ',' || echo "0,0,0,0"
}

pt() {
    powershell -NoProfile -Command "
\$ErrorActionPreference='SilentlyContinue'
Get-Process sanvi-api,sanvi-worker | ForEach-Object {
  '{0}|{1}|{2}|{3}|{4}' -f \$_.Name, [math]::Round(\$_.WorkingSet64/1MB,1), \$_.Threads.Count, \$_.HandleCount, [math]::Round(\$_.TotalProcessorTime.TotalMilliseconds,0)
}" 2>/dev/null | tr -d '\r'
}

host_cpu() {
    typeperf "\Processor(_Total)\% Processor Time" -sc 1 2>/dev/null | grep '^"[0-9]' | sed 's/.*,//; s/"//g; s/\r//'
}

echo "ts,host_cpu,api_rss_mb,worker_rss_mb,api_threads,worker_threads,api_handles,worker_handles,api_cpu,worker_cpu,pool_size,pool_idle,acq_count,acq_sum,acq_fail,pg_total,pg_api,pg_worker,pg_active" >> "$outfile"

prev_api_cpu=0; prev_wrk_cpu=0
prev_ts=$EPOCHREALTIME
start=$SECONDS
while :; do
    now=$SECONDS
    if (( max_secs > 0 && now - start >= max_secs )); then break; fi

    ts_now=$EPOCHREALTIME
    elapsed=$(awk -v a="$ts_now" -v b="$prev_ts" 'BEGIN{d=a-b; print (d>0.5?d:0.5)}')
    prev_ts=$ts_now

    hc="$(host_cpu)"
    proc="$(pt)"
    api="$(printf '%s\n' "$proc" | grep '^sanvi-api|' | head -1)"
    wrk="$(printf '%s\n' "$proc" | grep '^sanvi-worker|' | head -1)"
    f() { printf '%s' "$1" | cut -d'|' -f"$2"; }
    api_rss="$(f "$api" 2)"; api_thr="$(f "$api" 3)"; api_hdl="$(f "$api" 4)"; api_cpu_ms="$(f "$api" 5)"
    wrk_rss="$(f "$wrk" 2)"; wrk_thr="$(f "$wrk" 3)"; wrk_hdl="$(f "$wrk" 4)"; wrk_cpu_ms="$(f "$wrk" 5)"
    api_cpu="$(awk -v d="$((api_cpu_ms - prev_api_cpu))" -v i="$elapsed" -v c="$cores" 'BEGIN{printf "%.1f", (d>0?d:0)/1000/i/c*100}')"
    wrk_cpu="$(awk -v d="$((wrk_cpu_ms - prev_wrk_cpu))" -v i="$elapsed" -v c="$cores" 'BEGIN{printf "%.1f", (d>0?d:0)/1000/i/c*100}')"
    prev_api_cpu="$api_cpu_ms"; prev_wrk_cpu="$wrk_cpu_ms"

    m="$(curl -s --max-time 5 http://localhost:8081/metrics | awk '
        /^db_pool_size_gauge / {size=$2}
        /^db_pool_idle_gauge / {idle=$2}
        /^db_pool_acquire_seconds_count / {cnt=$2}
        /^db_pool_acquire_seconds_sum / {sum=$2}
        /^db_pool_acquire_failures_total / {fail=$2}
        END {printf "%s,%s,%s,%s,%s", size+0, idle+0, cnt+0, sum+0, fail+0}')"

    p="$(pg)"

    printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$(date +%H:%M:%S)" "$hc" \
        "${api_rss:-0}" "${wrk_rss:-0}" "${api_thr:-0}" "${wrk_thr:-0}" "${api_hdl:-0}" "${wrk_hdl:-0}" \
        "${api_cpu:-0}" "${wrk_cpu:-0}" "$m" "$p" >> "$outfile"

    if [[ "$stop_on_k6" == "yes" ]] && (( now - start > interval )) && ! tasklist //FI "IMAGENAME eq k6.exe" 2>/dev/null | grep -q k6.exe; then
        break
    fi
    sleep "$interval"
done
