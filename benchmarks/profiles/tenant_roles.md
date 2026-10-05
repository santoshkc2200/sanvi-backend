# `GET /api/v1/tenant/roles (host)` - before-number profile

**Rank:** #13 of 32 probed (requests x avg latency) · **Lab posture:** auth boundary (401)
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`tenant_roles/rc0.json`](tenant_roles/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 602 | 5.15 ms | 4.37 ms | 10.77 ms | 14.65 ms | 0.0000 |

**Server-side attribution (from [`tenant_roles/statements-rc0.txt`](tenant_roles/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

401-boundary shape: resolution + authn rejection only.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
