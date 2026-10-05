# `GET /api/v1/tenant/domains (host)` - before-number profile

**Rank:** #11 of 32 probed (requests x avg latency) · **Lab posture:** auth boundary (401)
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`tenant_domains/rc0.json`](tenant_domains/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 602 | 5.35 ms | 4.32 ms | 10.75 ms | 14.30 ms | 0.0000 |

**Server-side attribution (from [`tenant_domains/statements-rc0.txt`](tenant_domains/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

401-boundary shape, same as the other tenant GETs: resolution lookup + middleware front half, no handler execution.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
