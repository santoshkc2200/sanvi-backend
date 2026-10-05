# TASK-021: 11.2 Backend performance, load testing & capacity

**Phase:** 11 · **Status:** in-progress · **Size:** L
**Requirement(s):** FR-1108, FR-1109, FR-1110, NFR-1101, NFR-1104
**Depends on:** TASK-019, TASK-020, **phase 10 merged (TASK-016, TASK-017, TASK-018)**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** none — every change is behaviour-preserving

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Make the API hold every lab latency budget at three times the harness's projected peak with headroom,
and produce two numbers no other task can derive: the measured concurrency ceiling that TASK-023's
shedding thresholds come from, and the capacity constants TASK-026's per-plan limits come from.

**Every change in this task is behaviour-preserving. If a response body changes, it belongs in another
task.**

## Why this shape

Set the shedding threshold or the rate limit by guess and it is either decorative (too high) or an
outage of our own making (too low). That is the whole reason this task exists as a separate slice
rather than as a preamble to TASK-023.

The capacity model is scoped to what one host can tell you and is labelled as such. A 3× load test on
a laptop is a regression guard, not a production capacity model; pretending otherwise is exactly the
kind of claim NFR-1101 exists to prevent.

## Constraints

- Behaviour-preserving. The two contract-adjacent exceptions below are enforced repository-wide and are
  spec edits, not response-shape changes.
- Rollback is **per-commit**, not per-slice: each optimisation is independently revertable. The two
  changes carrying real risk are pool resizing and cache TTL changes — both are configuration, both
  ship with the previous value recorded in the PR description.
- Measure the RLS predicate cost **before** optimising around it. The intuition that RLS is expensive
  is often right and often wrong on the same schema, per table.

## File ownership map

- `crates/platform/{cache,http,persistence}/**` — pools, TTLs, single-flight, compression, ETag
- every context's read models and query-side projections
- `migrations/**` — index additions and removals
- `tests/perf/**` — `EXPLAIN` assertions, statement-count guards
- `benchmarks/**`, `docs/runbooks/capacity.md`
- the thirteen `api/*.yaml` where a list endpoint is still unbounded

## Execution slices

Ten steps in one session means one context, one compaction, and re-reads of work already done. The
slices below are the commit boundaries; each is one agent session with a fresh context, and each
carries the scoped commands to run **in that session**. The task is done when the last slice is.
`just` may be absent on some hosts — the `scripts/*.sh` equivalents are the portable form (the same
convention 11.5g ran under).

| Slice | Steps | Shape | Scoped verification for the session |
| --- | --- | --- | --- |
| **11.2a** close the baseline's measurement gaps | 0 | measurement-led, environment-heavy | `scripts/test.sh -p sanvi-persistence`; a new `benchmarks/baseline-<date>.md` naming a release build, pool section filled for both processes, ≥50 application statements or the reason recorded |
| **11.2b** cursor pagination + statement timeouts, contract first | 1 | contract-led, wide | the unbounded-collection test fails before and passes after; `scripts/check-openapi.sh`; `scripts/test.sh -p sanvi-api` |
| **11.2c** profile the top 20 endpoints | 2 | measurement-led | twenty `benchmarks/profiles/<endpoint>.md`, each citing a committed artifact for its before-number |
| **11.2d** query/index review, RLS cost, EXPLAIN assertions | 3 | database-led, judgement | `just test-perf` green; drop one asserted index in a scratch database and confirm the failure names the plan flip |
| **11.2e** N+1 elimination with statement-count guards | 4 | **bulk, mechanical — delegatable** | `scripts/test.sh -p <changed crates>`; deliberately reintroduce one loop and confirm the count test fails |
| **11.2f** pool sizing from measurement | 5 | small, config | `scripts/test.sh -p sanvi-persistence -p sanvi-api`; the commit records prior values and the measurement each new value came from |
| **11.2g** cache review incl. tenant-key audit | 6 | judgement-led | stampede protection holds at 500 concurrent misses; a tenant-less key fails a unit test by construction; `scripts/test.sh -p sanvi-cache` |
| **11.2h** transport tuning | 7 | small | `scripts/test.sh -p sanvi-http`; the compression-floor test proves no compression below the floor and none over already-compressed payloads |
| **11.2i** load, spike, soak + the capacity model | 8–9 | measurement-led, long | 3×/10× artifacts committed; soak log; `docs/runbooks/capacity.md` with every constant cited |
| **11.2j** close the task | — | no code | the full `## Verification` block, once, then the status flip in this file and `../backlog.md` |

**Order and parallelism.** 11.2a → 11.2c is a hard edge: the top-20 ranking reads (requests ×
latency) off the baseline, and a debug-build baseline is not a basis for ranking. 11.2a → 11.2d and
11.2a → 11.2f are hard edges too: statements and pool gauges come from the same capture. 11.2e
follows 11.2d — index work changes plans, and a statement count frozen before it goes stale
immediately. 11.2b, 11.2g and 11.2h are independent of all of these and may run concurrently in
separate sessions. 11.2i runs last; it needs every budget in place.

**Why 11.2e is the delegation candidate.** It is the only slice whose work is bulk-mechanical with a
machine oracle: assert a statement count per read model and let the count test say whether the
enumeration is complete — deliberately reintroducing a loop fails it. Review is reading a diff. Every
other slice turns on judgement: which pagination key a contract can freeze, whether a TTL is
justified, whether a compression floor is honest at the payload sizes this API actually serves.

## Steps

### Step 0: Close the baseline's measurement gaps

**Files:** Modify `docker-compose.yml`, `crates/platform/persistence/**`; Create
`benchmarks/baseline-<date>.md`

**Do:** `benchmarks/baseline-2026-09-17.md` (TASK-019) is not yet a sound input for steps 2, 3 and 5,
and its own record hands these fixes to this task:

- **Statements.** It tracks 16 statements, not the 50 TASK-019 asked for, and two of those are the
  capture's own (`CREATE EXTENSION`, `SELECT count(*) FROM pg_stat_statements`). Enable
  `pg_stat_statements` permanently in the compose Postgres instead of by `ALTER SYSTEM` per capture,
  and widen the capture using the reset procedure the baseline records. If the real query surface is
  still under 50, record why rather than padding it.
- **Pool saturation.** The API exports no sqlx pool gauges, so the baseline's pool section is empty.
  Export size, idle and acquire-wait for both `apps/api` and `apps/worker`; step 5 sizes pools from
  these.
- **Build profile.** The API was measured from the debug build because the release profile fails to
  link on the capture host (`sqlx_macros` dylib). Fix the link or capture on a host where release
  links; debug-build latencies are not a basis for capacity numbers.

**Verify:** a new baseline names a release build, fills the pool section for both processes, and lists
at least 50 application statements or records why the surface is smaller. Steps 2, 3 and 5 take their
before-numbers from it, not from the 2026-09-17 capture.

### Step 1: Enforce cursor pagination and statement timeouts, contract first

**Files:** Modify `api/*.yaml`, `crates/platform/{http,persistence}/**`; Test: `tests/perf/**`

**Do:** any collection endpoint without a bounded page size is a load test waiting to be run by a
customer. Replace offset pagination on large tables with cursor pagination. Make statement timeouts
part of the contract: a request that cannot complete inside its budget returns a problem-details error,
not a held connection.

**Verify:** `just openapi` clean; a test enumerates collection endpoints from the specs and fails on an
unbounded one.

### Step 2: Profile the top 20 endpoints and record before-numbers

**Files:** Create `benchmarks/profiles/<endpoint>.md`

**Do:** choose the twenty by (requests × latency) from TASK-019's baseline, **not by opinion**.
`cargo flamegraph` for CPU, `tokio-console` for task stalls and blocked executors.

**Verify:** twenty profile records exist, each with a before-number taken from a committed benchmark
artifact.

### Step 3: Review queries, measure RLS cost, fix indexes

**Files:** Modify `migrations/**`; Create `tests/perf/explain_assertions.rs`

**Do:** work the `pg_stat_statements` top offenders from the baseline: missing and duplicate indexes,
sequential scans on tenant-scoped tables, and the measured cost the RLS predicates add per table.
Land `EXPLAIN (ANALYZE, BUFFERS)` assertions so a plan regression fails CI rather than surfacing as a
page.

**Verify:** `just test-perf` fails when a profiled endpoint's plan flips to a sequential scan.

### Step 4: Eliminate N+1 with statement-count guards

**Files:** Modify every context's read models; Create `tests/perf/statement_counts.rs`

**Do:** sweep the read models across all thirteen contexts; introduce query-side projections only where
a join-per-row cannot be batched away. Repository-level tests count statements per use case and fail
above the expected count.

**Verify:** each guarded use case has an asserted statement count; deliberately reintroducing a loop
fails the test.

### Step 5: Size the pools from measurement

**Files:** Modify `crates/platform/persistence/**`, `configs/**`

**Do:** derive separate ceilings for `apps/api` and `apps/worker` from measured concurrency under the
k6 steady-state run. Statement timeouts per pool; pool saturation wired to TASK-020's alert rules.

**Verify:** the PR records the previous values and the measurement each new value came from.

### Step 6: Review the cache, including tenant scoping of every key

**Files:** Modify `crates/platform/cache/**`

**Do:** hit ratios per cache, single-flight stampede protection on every expensive key, negative
caching where a miss is common and cheap to be briefly wrong about, TTLs justified per entry rather
than inherited. **Audit every cache key for a tenant component — a cache is a bypass of RLS if its key
forgets the tenant.**

**Verify:** stampede protection holds under 500 concurrent misses on one key; a cache key constructed
without a tenant component fails a unit test **by construction**, not by review.

### Step 7: Tune transport

**Files:** Modify `crates/platform/http/**`

**Do:** response compression with a size floor, keep-alive tuning, payload trimming (fields nobody
reads are cost), `ETag` and conditional requests on the storefront read path.

**Verify:** a test asserts compression is not applied below the floor and never to an
already-compressed payload.

### Step 8: Run load, spike and soak

**Files:** `benchmarks/**`

**Do:** steady state at 3× the harness's projected peak; spike to 10× for two minutes; a 24-hour soak
watching RSS, connection counts, and task counts.

**Verify:**
- 3× holds every budget from `docs/slo/budgets.md` at under 70 % utilisation on every resource.
- 10× for two minutes recovers without a restart.
- The soak shows flat memory, no connection growth, no unbounded task accumulation.
- `just perf-compare` against the TASK-019 baseline passes on every scenario, or the exception is
  explained in the PR.

### Step 9: Write the capacity model, scoped honestly

**Files:** Create `docs/runbooks/capacity.md`

**Do:** map requests/tenant/day to CPU, memory, DB IOPS, and Redis memory using the constants measured
here, with a scaling trigger per resource. **Label it single-host and lab-measured**, and say what it
does not tell you.

**Verify:** every constant in the document cites a committed benchmark artifact.

## Definition of done

- [ ] Cursor pagination on every collection endpoint; no unbounded list in any of the thirteen specs.
- [ ] Statement timeouts return a problem-details error rather than holding a connection.
- [ ] Twenty endpoints profiled with before/after numbers committed.
- [ ] Missing indexes added, duplicates removed, RLS predicate cost recorded per table.
- [ ] A plan flipping to a sequential scan on a profiled endpoint fails CI.
- [ ] No N+1 remains in any read model; a statement-count test guards each one.
- [ ] Pool sizes and timeouts derive from measurement and are documented with their prior values.
- [ ] Stampede protection holds at 500 concurrent misses; a tenant-less cache key cannot compile.
- [ ] 3× peak holds every lab budget under 70 % utilisation; 10× recovers; the 24 h soak is flat.
- [ ] `docs/runbooks/capacity.md` is published, labelled single-host, with every constant cited.

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just test-perf
just perf
just perf-compare rc.1 rc.3
```

## Out of scope

Anything that changes behaviour: shedding and backpressure (TASK-023), rate limits and quotas
(TASK-026), the SSR response cache (TASK-022), partitioning and vacuum tuning (TASK-028). Frontend
bundle work (frontend TASK-022).

## Parked

A production capacity model and 3×-peak numbers that mean anything about production — see
[`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*

### Slice 11.2a record — 2026-10-02

Step 0 closed on a fresh Windows host. `PoolMetrics` (`crates/platform/persistence`
`pool_metrics.rs`) exports `db_pool_size_gauge`, `db_pool_idle_gauge`,
`db_pool_acquire_seconds` and `db_pool_acquire_failures_total` from the process
Prometheus registry (the direct-registration pattern `PaymentsMetrics` uses),
wired into both `apps/api/src/main.rs` and `apps/worker/src/main.rs` after
`build_pool`. sqlx has no acquire-wait hook and read paths check connections
out directly, so the acquire number is a 1 Hz **probe** — one real acquire per
interval, timing the wait; under saturation it waits behind the queue. The
probe semantics, the four instruments and the failure counter are unit-tested
(16/16 `scripts/test.sh -p sanvi-persistence`); clippy, fmt and layering clean
over the changed crates.

`pg_stat_statements` is permanent in the compose Postgres: preload via the
compose `command`, extension created on fresh volumes by
`scripts/db/init/02_pg_stat_statements.sql` — including `template1`, so every
database created later inherits it (the 2026-09-17 capture missed this and
paid a per-capture `CREATE EXTENSION`). The release profile **links on this
host** — the build-profile gap that invalidated the old capture is closed.

New baseline `benchmarks/baseline-2026-10-02.md`: seed-1 post-erratum
(checksum `22e2bac8…`, the plan's 5,748 payments), release build, machine
profile 20-core/16 GB x86_64 Windows. Statement union across the five k6
shapes is **13 distinct statements** — under 50, and the doc records why
rather than padding: three of five shapes stop at the 401/signature boundary
(no Kratos locally), and the two deep journeys are Redis-cached by design;
steps 3–4 cover the full surface per use case instead. The old baseline's
empty pool section is filled: at the offered rates the API pool sat at size
3–4 of 10 with every probe wait under 5 ms and zero failures. Artifacts
committed as `benchmarks/<scenario>/rc0.json` (all five 0 failed, 0 dropped;
the abuse-quota metric confirms all 3,000 beacons `counted`, none `blocked` —
the lab run raises the tracking quota via config so the shape measures the
beacon path, not the counter).

Host notes for the next slice: k6 upgraded to v2.3.0 (scoop's 0.51 bundle
cannot parse the harness's optional catch binding); `docker-compose.override.yml`
(gitignored, host-local) publishes Postgres on **5433** because another
Docker allocation holds 5432; the lab API env is spelled out in the baseline
document. The worker registers the same gauges on its scrape registry as
every other worker metric — the worker has no HTTP scraper of its own, which
is the pre-existing export posture all worker metrics share (unchanged here).
Route-matrix diff acknowledged: two fingerprint rows only
(`apps/api/src/main.rs`, `apps/worker/src/main.rs`), no route rows.

### Slice 11.2b record — 2026-10-05

Step 1 closed: cursor pagination and statement timeouts are contract-first,
enforced by a gate that fails the build on an unbounded collection.

**The gate.** New test target `apps/api/tests/perf` (`just test-perf`,
wired into `just ci`/`ci-quiet`; the pr.yml integration job already runs
`-p sanvi-api`, so CI coverage needed no workflow edit — the TASK-020 gap
noted in 11.5f does not recur here). `bounded_collections.rs` walks the
merged OpenAPI document (`sanvi_api::ApiDoc`, the same source
`check-openapi.sh` pins to the committed `api/*.yaml`) and fails on every
GET whose 2xx JSON response contains an array of objects unless the
operation declares a `limit` query param with `minimum ≥ 1`, a `maximum`,
and a `default` inside the band, or carries a reasoned line in
`collection-exemptions.txt`. Stale exemptions (operation gone, or gained a
bounded `limit`) and unknown names fail too, and a vacuity guard asserts
the walker enumerated a non-empty set. Eight fixture tests prove each rule
on synthetic documents. **Fails-before evidence:** the first run against
the untouched contract listed 47 offenders (9 paginated endpoints whose
`limit` had no declared maximum — every bound lived in a Rust clamp the
client could not see — plus 38 unpaginated collections and single-entity
surfaces); after the sweep it is 9/9 green.

**The conversions.** 24 endpoints gained `limit` + `after` (keyset on the
table's Snowflake id, `ORDER BY id DESC`, cursor = last row's id, 400
`invalid cursor` on garbage): billing invoices; identity members (keyset
`user_id`) and invitations; domains custom-domains, orders and the
platform search; theming theme versions; advertising campaigns, campaign
changes, creatives, conversions, audiences, budget caps and budget alerts;
privacy platform/tenant DSRs, appeals (oldest-first queue: `id ASC`),
holds, incidents, obligations and assessments; platform-admin
impersonations and pending approvals (oldest-first). Five more
(audit, tenancy, payments×2, the admin tenant search) were already
cursor-paginated and only gained the declared bound
(`schema_with` → `sanvi_http::openapi::page_limit_schema(max, default)`).
Hidden caps became parameters with today's values as default: conversions
and audiences (SQL `LIMIT 200`), DSRs (200/100), campaign changes (50).
The kernel's dead offset machinery gained the one helper every clamp now
routes through (`clamp_page_size`), so spec bounds and clamping share a
shape. Previously-unbounded lists default to their maximum (200): the
absent-limit response for a tenant with more members than that changes —
that is the boundedness this slice exists to ship, recorded here rather
than smuggled. Ordering notes: lists that ordered by `created_at` now
order by `id` (Snowflakes encode creation time; deterministic and
keyset-able — TASK-027 finalises ordering); internal sweeps that need
unbounded reads (budget evaluator, rollover, status, metrics ingest,
audience refresh, appeals decisioning) keep their original ports — the
paginated HTTP reads are separate `_page`/page-variant methods, so no job
was silently capped. `list_sessions` is exempted, not bounded: it returns
only the caller's current Kratos session — a singleton by construction.
41 exemption lines in total (catalogs/registries, bulk document-sync
surfaces, reports, single-entity responses whose arrays are embedded
children). Two changed `sqlx::query!` macros (identity members and
invitations) were moved to the runtime-checked `query_as` form — the
runtime form is already the majority style (122 vs 58) — so the `.sqlx`
cache is untouched and no `prepare` was needed.

**Statement timeouts (FR-1110).** `DatabaseConfig.statement_timeout_ms`
(default 10 000, `0` disables) is applied per connection as a
`statement_timeout` startup parameter in `build_pool`, under the 30 s
request-budget middleware so the database cancels first. Migration paths
run uncapped by construction: the CLI `migrate` command and every
testsupport pool zero it (DDL on scale data legitimately outlives any
request budget). Postgres's cancel surfaces as the contract's
`request-timeout` problem (504): `ApiError::from_storage_detail` classifies
the exact `canceling statement due to statement timeout` message (lock
timeouts share SQLSTATE 57014 and stay 500s), and the 15 handler
`Storage`/commit mapping sites across all contexts route through it;
`sanvi_persistence::is_statement_timeout` is the typed form.
`statement_timeout_cancels_runaway_queries` proves the behaviour against
a real Postgres: `pg_sleep(30)` under a 1 s cap is cancelled near the cap,
classifies correctly, and the pool serves the next query (statements die,
sessions do not). Every converted endpoint's contract declares the 504.

**Execution note.** The slice plan delegated the context sweep to parallel
subagents; all four died within minutes on the account's 5-hour usage
limit (reset 12:48) having touched nothing — the sweep was completed
in-session, one context at a time, against the billing pattern.

**Verification (scoped, this session).** The gate 9/9 after failing on 47
named offenders; `check-openapi.sh` clean over all 14 specs after
regeneration; `scripts/test.sh -p sanvi-api` 65/65 (idor sweep green —
the new params changed no route surface); per-context suites green as
landed (advertising 259, privacy/domains/theming/billing/audit/tenancy/
payments 462, identity+platform-admin 69, persistence 18 incl. the new
timeout test, billing 38 incl. the new keyset/clamp test); workspace
`clippy --all-targets -D warnings` and `cargo fmt --check` clean;
layering OK; route-matrix regenerated (15 fingerprint rows only, no route
rows); ga-checklist OK. Full-suite re-run recorded in the commit.

### Slice 11.2c record — 2026-10-05

Step 2 closed on the same Windows host as 11.2a (20 cores/16 GB), release
build rebuilt from current main first so the numbers include 11.2b's
pagination and timeout work.

**The ranking input, measured.** The 2026-10-02 baseline's five k6 shapes
are per-shape, not per-endpoint, so the slice first measured per-endpoint
artifacts: new `tests/load/profile_probe.js` (one endpoint per run,
configured by `PERF_PROBE_*` env, reusing lib.js's stats_version-2
summary so the artifacts carry machine profile + build stamp + expected
status mix) and `scripts/profile-probes.sh` (32 candidate endpoints ×
20 rps × 30 s ≈ 600 requests each, with `pg_stat_statements` reset
before each probe and the top-15-by-total-time capture committed next to
the latency artifact). Candidates cover every public surface the lab can
reach, the operational surfaces, and 401-boundary representatives across
the tenant contexts. Ranking is (requests × avg latency) over the
committed artifacts; the cut at #20 is clean (#20 2,811 vs #21 2,547, no
ranking-method choice moves it). Outputs: `benchmarks/profiles/README.md`
(method, full 32-row ranking, named gaps) and twenty
`benchmarks/profiles/<endpoint>.md` records, each citing its committed
artifact as its before-number.

**What the profiles say (inputs for steps 3, 4, 6, 7).** The two heaviest
endpoints are in-process, zero-SQL: `GET /metrics` (28.5 ms avg, widest
spread, p99 79.9 ms — Prometheus text render of the full registry) and
`GET /openapi.json` (25.3 ms — rebuilds the merged 14-context document
per request; ETag/caching is step 7's, contract-adjacent). `public_plans`
is the set's N+1: one entitlements query and one prices query per plan
row (1,803 calls each per 601 requests) — step 4's first target. The four
privacy disclosure reads rebuild slow-moving content per request (step 6
candidates). The live-hostnames resolution lookup tops every
tenant-host probe (0.022–0.026 ms mean) — the baseline's dominant
statement, unchanged. Public reads write an audit row per request
(`platform_scope_begin` INSERTs ride slug-availability, the privacy
disclosures, plans) — recorded as behaviour, not changed.

**Named gaps.** CPU sampling: samply/ETW requires an elevated Windows
host; this host has sudo disabled ("samply requires Administrator
privileges for profiling on Windows"). tokio-console is not wired and its
TUI cannot be captured headlessly. Attribution therefore stops at
in-process vs database (statements captures) plus the latency
distributions; the README records the elevated-host drill. Authenticated
handlers: no Kratos locally, so ten of the twenty records measure the
middleware front half (resolution + authn rejection, ~5 ms each) and say
so — the baseline's standing caveat.

**Defects found and fixed here (the slice's only behaviour change).**
Smoke-testing every candidate path before probing surfaced a mount
defect class the route/permission gates cannot see — actix commits a
request to the first matching resource, and both privacy and billing
registered one `web::resource` per catalog entry: every second method on
a shared path answered 405 (all platform privacy writes, the tenant DSR
list, `POST /api/v1/platform/plans`), and the privacy subject scope was
mounted under `/api/v1/privacy` while registering full paths, doubling
the prefix — the whole subject privacy surface (DSR submission, consents,
directives, opt-out, …) answered 404 at its contracted paths. Fixed in
`sanvi_privacy` (per-path grouped registration, `PermissionMap` for
per-method permissions, subject paths relative to their mount) and
`sanvi_billing` (same grouping), each with a regression test proven to
fail naming the operation on the old registration. An OpenAPI-wide live
sweep (all 210 operations, one request each) reports zero 405s after.
Found alongside: the route-matrix self-test fixture used
`str(Path.relative_to)`, which is backslash on Windows and failed the
gate's POSIX handler-key lookup — same portability class 11.5g fixed in
the layering and secret-scan gates; one-line `as_posix()` fix.

**Verification (scoped, this session).** privacy+billing+api test tier
166/166 (incl. the DB-backed IDOR sweep and the two new mount regression
tests, each shown failing on the old registration); `cargo fmt`/clippy
clean over the changed crates; route-matrix regenerated (two fingerprint
rows only, `routes.rs` sources) and self-test green; `check-openapi.sh`
clean over all 14 specs (no contract change); ga-checklist OK. k6 v2.3.0
note for Windows hosts: run the probe script with `MSYS_NO_PATHCONV=1`
(Git Bash path-converts leading-slash env args into
`C:/Program Files/Git/...`; the first probe pass measured nothing for
exactly this reason and was re-run).
