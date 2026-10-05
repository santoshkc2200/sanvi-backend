# `GET /api/v1/public/plans` - before-number profile

**Rank:** #3 of 32 probed (requests x avg latency) · **Lab posture:** public read, deep
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`public_plans/rc0.json`](public_plans/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 602 | 7.94 ms | 7.06 ms | 14.90 ms | 16.99 ms | 0.0000 |

**Server-side attribution (from [`public_plans/statements-rc0.txt`](public_plans/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

The only deep public read with a fan-out problem: per request the plans list (1 query) is followed by one `plan_entitlements` query and one `plan_prices` query *per plan row* (1,803 calls each against 601 requests = 3 rows per request). That is the N+1 shape step 4 (11.2e) hunts; a batched read model (entitlements/prices joined or `WHERE plan_id = ANY($1)`) collapses 7 statements to 1-2. Also: an audit_log INSERT rides every request (601 calls) - a public read writes audit rows, recorded here as behaviour, not changed (this task is behaviour-preserving).

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
