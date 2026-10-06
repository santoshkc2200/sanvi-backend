# Runbook — connection pool saturation (api & worker)

**Alerts:** `DbPoolAcquireWaitHigh`, `DbPoolAcquireFailures`
(see [latency-budget-breach](./latency-budget-breach.md) for the request-side
symptom of the same cause).

The api and the worker each hold their own sqlx pool against the same
database — the api's ceiling is `[database].max_connections`, the worker's is
`[worker_pool].max_connections` applied over the same section (TASK-021
11.2f). Both export the same four instruments
(`crates/platform/persistence` `PoolMetrics`): `db_pool_size_gauge` (open
connections), `db_pool_idle_gauge` (idle among them),
`db_pool_acquire_seconds` (wait a probe acquire experienced) and
`db_pool_acquire_failures_total` (probe acquires that failed).

**The acquire number is a probe, not a hook.** sqlx exposes no acquire-wait
callback and read paths check connections out directly, so a sampler probes
each pool once a second with one real acquire and records how long that wait
took. Under saturation the probe waits behind the queue — that is the point.
Steady state on the lab baseline is a mean under 1 ms with everything in the
≤ 5 ms bucket and zero failures.

## 1. Assess

**Which process is saturated?** The two pools present distinct
`application_name`s (`sanvi-api`, `sanvi-worker`), so Postgres itself
attributes connections:

```sql
SELECT application_name, state, count(*), max(now() - state_change) AS longest_state
FROM pg_stat_activity
WHERE datname = current_database()
GROUP BY 1, 2;
```

- `sanvi-api` rows with `state = 'active'` and a growing `longest_state`:
  the database is the bottleneck — find the statement below.
- Pool size at its ceiling with idle at 0 while every session is `idle`:
  connections are checked out but not running statements — a handler is
  holding a connection across slow non-database work. That is a code defect,
  not a sizing problem; raising the ceiling only hides it.
- `state = 'idle in transaction'`: a transaction was abandoned before
  commit/rollback. Check for error paths that drop a `TxManager` without
  rolling back.

**What is running?** For the saturated process's sessions:

```sql
SELECT pid, application_name, now() - query_start AS running_for, left(query, 120)
FROM pg_stat_activity
WHERE datname = current_database() AND state <> 'idle'
ORDER BY query_start;
```

and the statement that accumulated the most time:

```sql
SELECT calls, round(max_exec_time::numeric, 1) AS max_ms,
       round(mean_exec_time::numeric, 1) AS mean_ms, left(query, 120)
FROM pg_stat_statements
ORDER BY max_exec_time DESC LIMIT 10;
```

A statement newly at the top with a sequential-scan plan on a profiled read
model is a plan regression — `just test-perf` pins those plans and names the
index that disappeared.

## 2. Fix

In order of preference:

1. **Fix the query.** A missing index, a plan flip, or an N+1 that survived
   the statement-count guards is the root cause; the pool alert is its
   shadow. `just test-perf` and the guards exist to catch both before merge.
2. **Fix the hold.** A connection held across a slow external call (HTTP,
   mail) is a defect regardless of pool size — move the call outside the
   transaction/pool scope.
3. **Raise the ceiling — deliberately, with the prior value in the PR.**
   The api's ceiling lives in `[database].max_connections`
   (`configs/base.toml`, overlay per environment); the worker's in
   `[worker_pool].max_connections`. Both were set from measurement
   (api 10: unchanged at the 3× probe, idle ≥ 8 of 10; worker 8: 2× the
   measured peak of 4) — a raise needs a number from the same gauges, not a
   guess, and a check against the database's own `max_connections` budget
   (api ceiling + worker ceiling + migrator/superuser sessions must fit
   under it). The production database's connection budget is a deployment
   fact, not a repo constant — record it in the capacity model before
   raising anything that touches production.

`db_pool_acquire_failures_total` page-grade means acquires were already
failing (the acquire timeout elapsed): expect failed requests/jobs in the
same window, not only a risk of them.

## 3. Verify

- `db_pool_acquire_seconds` p95 back into the ≤ 5 ms bucket; the failure
  counter stays flat.
- `pg_stat_activity` shows the saturated process with idle connections
  again (idle ≈ size).
- If a ceiling was raised: the new value and its measurement are recorded in
  `benchmarks/pool-sizing-2026-10-06.md`'s follow-up section, and the
  capacity model's connection row cites it.
