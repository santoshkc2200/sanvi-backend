# `GET /api/v1/public/privacy/notice-at-collection` - before-number profile

**Rank:** #5 of 32 probed (requests x avg latency) · **Lab posture:** public read, deep
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`public_notice-at-collection/rc0.json`](public_notice-at-collection/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 601 | 6.35 ms | 5.54 ms | 12.84 ms | 15.88 ms | 0.0000 |

**Server-side attribution (from [`public_notice-at-collection/statements-rc0.txt`](public_notice-at-collection/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

Same shape as the privacy notice: retention-rule rows rebuilt per request (0.054 ms mean) plus the per-request platform_scope_begin audit INSERT. Step 6 caching candidate.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
