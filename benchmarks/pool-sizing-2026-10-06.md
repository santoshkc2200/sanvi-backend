# Pool sizing from measurement — TASK-021 step 5 (slice 11.2f)

**Lab, not production.** Same posture as [`baseline-2026-10-02.md`](baseline-2026-10-02.md):
seed 1 scale data (`sanvi_scale`, 5,748 payments), compose Postgres 18 on a
Windows developer host (20 cores / 16 GB, port 5433), release build
(`0.10.0`, unstamped local build — the pipeline stamps release binaries),
Redis resolution/theme caches warm, rate limiting disabled, tracking abuse
quota raised. Nothing here sizes a production database; the production
connection budget is a deployment fact recorded in
[`docs/release/needs-humans.md`](../release/needs-humans.md).

- **Date:** 2026-10-06
- **Runs:** the five steady-state shapes at their offered rates
  (storefront_read 50 rps, admin_write 20, checkout_create 10,
  webhook_flood 100, conversion_upload 50 — 60 s each, k6 v2.3.0, all five
  0 failed / 0 dropped, rc 0), then two 3× sizing probes
  (storefront_read 150 rps, webhook_flood 300 rps — rc 0). The 3× artifacts
  are committed as `benchmarks/storefront_read/pool-sz.json` and
  `benchmarks/webhook_flood/pool-sz.json`; the five offered-rate latency
  artifacts carry no new information over the baseline's `rc0` set and are
  not duplicated here.
- **Pool observation:** a 2 s sampler (committed raw as
  [`pool-sizing-2026-10-06/samples.txt`](pool-sizing-2026-10-06/samples.txt))
  recorded, per sample: the api's four `db_pool_*` gauges from
  `GET /metrics`, and `pg_stat_activity`'s connection count for
  `application_name = 'sanvi'`. The worker's pool is the difference
  (total − api size): the worker has no HTTP scraper of its own — the
  pre-existing export posture all worker metrics share — so during this
  capture the two processes were separated by arithmetic. **From this slice
  on the pools present distinct `application_name`s** (`sanvi-api`,
  `sanvi-worker`, the new `[database].application_name` /
  `[worker_pool].application_name` config), so future captures attribute
  connections directly.

## What was measured

Per-shape maxima over the sampler's 2 s samples; "api busy" is
`size − idle` at the sampled instants (the pool holds up to 10 open
connections; busy is the demand signal), "worker" is
`pg_stat_activity total − api size`:

| run | api busy (max) | api idle (min) | worker conns (max) | probe failures | mean probe wait |
|---|---:|---:|---:|---:|---:|
| storefront_read @ 50 rps | 3 | 7 | 3 | 0 | 3.6 ms¹ |
| admin_write @ 20 rps | 0 | 10 | 4 | 0 | 1.7 ms¹ |
| checkout_create @ 10 rps | 0 | 10 | 4 | 0 | 1.6 ms¹ |
| webhook_flood @ 100 rps | 0 | 10 | 4 | 0 | 1.2 ms¹ |
| conversion_upload @ 50 rps | 0 | 10 | 4 | 0 | 1.3 ms¹ |
| **storefront_read @ 150 rps (3×)** | **5** | **5** | 4 | **0** | 1.0 ms¹ |
| **webhook_flood @ 300 rps (3×)** | 0 | 10 | 4 | 0 | 1.4 ms¹ |

¹ Cumulative histogram mean over the shape's window
(`Δsum / Δcount` of `db_pool_acquire_seconds`), which includes connection
warm-up at the window's start. The session's final histogram is the cleaner
statement: **744 probes, 94 % inside the ≤ 5 ms bucket, every probe inside
≤ 50 ms, zero failures.**

Two structural notes, so nobody over-reads the zeros:

- webhook_flood is the signature-boundary shape (no Stripe locally): its
  requests do tenant resolution and stop — the pool demand it shows is the
  front half only. The 3× probe on it is a floor, not a ceiling.
- The api's pool **opened all 10 of its connections** during the offered-rate
  storefront run (bursts of concurrent acquisitions open connections on
  demand) while `idle` stayed ≥ 7 at every sample — holds are milliseconds,
  so no queue ever formed.

## Statement ceiling for the worker's budget

`pg_stat_statements` was reset immediately before the 3× window and read
after; the highest `max_exec_time` of any statement in that window is
**0.2 ms** (`domains.live_hostnames` lookup), with the worker's own
statements (outbox batch reads, outbox count, audit inserts, payments
gauges) all ≤ 0.1 ms. Every job statement measured sits five orders of
magnitude under the worker's new 60 s budget — the budget is a bound against
a runaway statement, not a tuned limit, and a batch statement that cannot
finish inside it needs chunking (TASK-028's archival work), not a bigger cap.

## The decision, with prior values

| value | process | prior | new | measurement it comes from |
|---|---|---:|---:|---|
| pool ceiling | api (`[database].max_connections`) | 10 | **10 — unchanged** | 3× storefront probe: busy ≤ 5 of 10, idle ≥ 5 at the worst sample, no acquire wait above the 50 ms bucket, zero failures |
| pool ceiling | worker (new `[worker_pool].max_connections`) | inherited 10 (base) / **50** (production/staging overlays) | **8** | max 4 open connections across every shape (outbox + scheduler + consumers); 2× the measured peak |
| statement timeout | api | 10 000 ms (implicit serde default) | **10 000 ms — now explicit in `[database]`** | unchanged; above every lab budget, under the 30 s request middleware (11.2b) |
| statement timeout | worker (new `[worker_pool].statement_timeout_ms`) | inherited 10 000 ms | **60 000 ms** | measured job-statement ceiling 0.2 ms; jobs are not request-shaped and legitimately run longer than any request budget (FR-1110's rationale is client-facing failure) |
| min connections | worker | inherited (1 base / 10 production) | **1 (inherited)** | jobs are interval-driven; connections open on demand at tick time |
| `application_name` | api / worker | both `"sanvi"` (hardcoded) | `sanvi-api` / `sanvi-worker` | pg_stat_activity attribution is the input every future pool capture reads |

The **load-bearing change is the split, not either ceiling**: before this
slice the worker took `[database]` whole, so the production overlay shipped
`max_connections = 50` *per process* — 100 against a default-100 Postgres
before migrator, superuser or session connections, an unexamined coincidence
the pool gauges never measured. After the split: production is 50 (api) + 8
(worker) = 58, and the worker's production floor drops from 10 warm
connections to 1 (jobs open on demand). The api's production ceiling of 50
remains a prior guess the lab cannot measure — it is recorded as such rather
than dressed up as derived.

## The derivation, stated honestly

- **api 10.** The pool is bounded above by useful concurrency, not traffic:
  one connection per in-flight request at a time (queries inside a handler
  are sequential; `TxManager` holds one per transaction), so the theoretical
  demand ceiling of this actix configuration is its worker-thread count
  (20) plus the probe. The measurement shows actual demand at 3× the
  harness's projected peak never exceeded 5 concurrent checkouts at any
  sampled instant, with no wait above the 50 ms bucket. A ceiling of 10
  covers the measured 3× demand at ≤ 50 % instantaneous utilisation —
  inside the phase's 70 % gate — and the 3× run in 11.2i re-checks it.
- **worker 8.** Job concurrency is interval-driven, not request-driven:
  measured peak 4 across every shape. 2× the peak absorbs overlapping
  schedules without queuing; 8 + 50 fits a default-100 production database
  with headroom for everything that is not an application pool.

## Defect found and fixed in this slice (measurement-led)

The worker **could not boot from the committed configs**: `[scheduler]` in
`configs/base.toml` never received the two TASK-017 (10.8) keys the
`SchedulerConfig` struct requires (`advertising_budget_guardrails_interval_secs`,
`advertising_budget_rollover_interval_secs`) — the section's serde form has
no per-field defaults, so `WorkerConfig::validate` failed before any pool
existed. Any environment without the env overrides spelling those keys was
running the api with no worker: outbox, schedulers, payments dispatch, the
lot. Fixed here by landing the struct's documented defaults in base.toml
(the file's convention for every other interval). This is also why the
worker's side of the 11.2a pool capture was never taken.
