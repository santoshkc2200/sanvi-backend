# `GET /api/v1/public/privacy/notice` - before-number profile

**Rank:** #4 of 32 probed (requests x avg latency) · **Lab posture:** public read, deep
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`public_privacy-notice/rc0.json`](public_privacy-notice/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 601 | 7.02 ms | 6.36 ms | 13.43 ms | 16.69 ms | 0.0000 |

**Server-side attribution (from [`public_privacy-notice/statements-rc0.txt`](public_privacy-notice/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

Per-request generation of slow-moving content: retention-rule rows (mean 0.054 ms, the largest single SQL line) + jurisdiction policy rows (0.043) + a `platform_scope_begin` audit INSERT (0.040) + the subprocessors read. The notice changes on compliance edits, not per request - the keyed-cache candidate for step 6 (11.2g).

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
