# `GET /api/v1/public/privacy/metrics` - before-number profile

**Rank:** #7 of 32 probed (requests x avg latency) · **Lab posture:** public read, deep
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`public_privacy-metrics/rc0.json`](public_privacy-metrics/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 602 | 5.58 ms | 4.83 ms | 11.38 ms | 14.67 ms | 0.0000 |

**Server-side attribution (from [`public_privacy-metrics/statements-rc0.txt`](public_privacy-metrics/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

Yearly request-metrics query per request (0.022 ms mean) plus the per-request audit INSERT (0.041). The underlying table changes at sweep cadence, not per request - step 6 caching candidate.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
