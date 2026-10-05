# `GET /metrics` - before-number profile

**Rank:** #1 of 32 probed (requests x avg latency) · **Lab posture:** operational surface
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`metrics/rc0.json`](metrics/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 601 | 28.46 ms | 23.79 ms | 50.26 ms | 79.86 ms | 0.0000 |

**Server-side attribution (from [`metrics/statements-rc0.txt`](metrics/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

No SQL at all (the capture holds only the capture's own reset). The whole ~28 ms is in-process: the Prometheus text render of the full registry. The widest latency spread in the set (p99 79.86 ms against avg 28.46) - this is the endpoint where in-process CPU sampling would decide the next move, and where the sampling gap bites hardest. A scrape every 15 s does not stress the host, but the render cost scales with registered series; the series count is the number to watch as instruments grow (step 5 wires pool gauges into the same registry).

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
