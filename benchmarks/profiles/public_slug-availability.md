# `GET /api/v1/public/slug-availability?slug=...` - before-number profile

**Rank:** #10 of 32 probed (requests x avg latency) · **Lab posture:** public read, deep
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`public_slug-availability/rc0.json`](public_slug-availability/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 602 | 5.40 ms | 4.71 ms | 10.98 ms | 14.15 ms | 0.0000 |

**Server-side attribution (from [`public_slug-availability/statements-rc0.txt`](public_slug-availability/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

The per-request `platform_scope_begin` audit INSERT (0.041 ms mean) costs more than the slug-existence query itself (0.035). The slug check is a three-way UNION over reserved slugs, reservations and tenants; at lab scale all hit indexes. The audit write per public read is the observation here.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
