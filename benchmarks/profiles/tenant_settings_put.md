# `PUT /api/v1/tenant/settings (host)` - before-number profile

**Rank:** #17 of 32 probed (requests x avg latency) · **Lab posture:** auth boundary (401), write
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`tenant_settings_put/rc0.json`](tenant_settings_put/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 601 | 4.95 ms | 4.13 ms | 10.43 ms | 13.73 ms | 0.0000 |

**Server-side attribution (from [`tenant_settings_put/statements-rc0.txt`](tenant_settings_put/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

PUT at the auth boundary - the same shape the admin_write load journey measures. ~5.0 ms front half; the write handler itself is behind Kratos.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
