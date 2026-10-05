# `GET /api/v1/tenant/theme/draft (host)` - before-number profile

**Rank:** #9 of 32 probed (requests x avg latency) · **Lab posture:** auth boundary (401)
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`tenant_theme-draft/rc0.json`](tenant_theme-draft/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 602 | 5.41 ms | 4.52 ms | 10.59 ms | 12.38 ms | 0.0000 |

**Server-side attribution (from [`tenant_theme-draft/statements-rc0.txt`](tenant_theme-draft/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

401-boundary shape: the request resolves the tenant (live-hostnames lookup, 0.026 ms mean) and is rejected by the authentication extractor before the handler runs. The handler's own cost is unmeasured in this lab (no Kratos) - the same named gap the baseline records. The ~5.4 ms is the middleware front half every authenticated journey pays before its handler.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
