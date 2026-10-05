# `GET /openapi.json` - before-number profile

**Rank:** #2 of 32 probed (requests x avg latency) · **Lab posture:** operational surface
**Probe:** 20 rps for 30 s against seed-1 scale data, release build, 2026-10-05
**Before-number (from [`openapi_json/rc0.json`](openapi_json/rc0.json), machine profile + build stamp inside):**

| requests | avg | p50 | p95 | p99 | unexpected-status |
|---:|---:|---:|---:|---:|---|
| 602 | 25.25 ms | 24.59 ms | 45.73 ms | 51.45 ms | 0.0000 |

**Server-side attribution (from [`openapi_json/statements-rc0.txt`](openapi_json/statements-rc0.txt)):**
`pg_stat_statements` reset immediately before the probe; top statements by total time after.

No SQL. The cost is rebuilding and serializing the merged 14-context OpenAPI document per request (~25 ms avg). The document is static for a given build, so a conditional request (ETag, step 7) or gateway caching would remove the cost entirely; left to step 7, since an ETag on the contract is contract-adjacent.

Reading guide: what this record is for and what the lab cannot measure here is in
[`README.md`](README.md). Behaviour-preserving task: nothing above is a change proposal by itself -
step 3 (indexes), step 4 (N+1) and step 6 (caching) take their inputs from these numbers, and the
after-number must come from the same probe at the same rate.
