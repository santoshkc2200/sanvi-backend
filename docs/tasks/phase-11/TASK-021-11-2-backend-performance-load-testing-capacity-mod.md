# TASK-021: 11.2 Backend performance, load testing & capacity

**Phase:** 11 · **Status:** done · **Size:** L
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

- [x] Cursor pagination on every collection endpoint; no unbounded list in any of the thirteen specs.
- [x] Statement timeouts return a problem-details error rather than holding a connection.
- [x] Twenty endpoints profiled with before/after numbers committed.
- [x] Missing indexes added, duplicates removed, RLS predicate cost recorded per table.
- [x] A plan flipping to a sequential scan on a profiled endpoint fails CI.
- [x] No N+1 remains in any read model; a statement-count test guards each one.
- [x] Pool sizes and timeouts derive from measurement and are documented with their prior values.
- [x] Stampede protection holds at 500 concurrent misses; a tenant-less cache key cannot compile.
- [x] 3× peak holds every lab budget under 70 % utilisation; 10× recovers. *The 24 h soak is
  **waived on this host by the task owner (2026-10-07): a developer machine, shared with the
  owner's other work, that entered idle standby mid-run on the first attempt — see
  [`benchmarks/soak-2026-10-07/README.md`](../../../benchmarks/soak-2026-10-07/README.md); the soak's
  pass conditions stand for whichever host eventually runs it.***
- [x] `docs/runbooks/capacity.md` is published, labelled single-host, with every constant cited.

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

### Slice 11.2d record — 2026-10-06

Step 3 closed on the same Windows host, all measurement against the seeded
scratch databases whose shape the plan gate now reproduces
(`benchmarks/explain-2026-10-06.md` is the record; numbers below are its).

**What the plans said.** 11.2b's keyset shape (`WHERE tenant_id … ORDER BY
id DESC LIMIT n`) is served by *primary-key walks with the tenant filter
riding on top* on five read models: invoices, DSRs, invitations,
custom_domains, orders — a 200-row invoice page read 6,000 rows and removed
4,000 by filter at three tenants, and the audit seq-keyset removed ~3,000
per 50-row page (the waste scales with platform size, not tenant size).
DSRs and invitations had no tenant index at all. The public
slug-availability check seq-scanned `tenancy.tenants` — `WHERE slug = $1`
cannot use the `lower(slug)` expression index. The RLS predicate measured
~0.1 µs/row as a filter and removes zero rows on every plan that leads with
`tenant_id` — the intuition that RLS is expensive is here wrong per table;
recorded rather than assumed.

**The migration** (`20261006000000_perf_keyset_indexes.sql`, down paired):
`(tenant_id, id)` keyset indexes created or realigned on the five read
models plus disputes (which missed the payments index pass of 20260824050000
despite an identical page query), `audit_entries_tenant_seq_idx`, the slug
unique index rebuilt on `(slug)` (the CHECK constraint makes the expression
form redundant), and `tenants_created_idx` for the platform browse keyset.
Behaviour-preserving: same rows, same order, same uniqueness. Reviewed and
deliberately left: translations (PK prefix), plan entitlements/prices (the
seq scan is correct at this distribution — public_plans' cost is the N+1,
step 4's), checkouts, the infix-ILIKE tenant search (pg_trgm is the
production answer if it ever grows; noted, not added).

**The gate** — `apps/api/tests/perf/explain_assertions.rs`, wired into
`just test-perf` (no workflow edit needed; the integration job already runs
`-p sanvi-api`). Fourteen rules pin the profiled read models' access paths:
each names a query as the adapter issues it, the table that must not be
scanned, and the index the plan must use. Pinning the index name — not
merely "no Seq Scan" — is load-bearing: the slice's drop-index drill
(required by the slice table) shows dropping `invoices_tenant_idx` flips
the plan to `Index Scan Backward using invoices_pkey` with the tenant as a
Filter, invisible to a scan-type check; the drill's plan and the checker's
failure message are both in the benchmark artifact, and fixture tests prove
the checker on synthetic plans.

**The fixture.** The rules must run in the steady-state posture (visibility
map filled), and the slice measured the hard way that a per-run clone
cannot: Postgres declines to mark freshly-committed pages all-visible, and
on an empty map the planner legitimately prefers the pkey walk for cursor
pages — identical statements flip on VACUUM/autovacuum alone. The gate
therefore builds a shared fixture database once per (migration set, seed)
pair under an advisory lock (the `sanvi_perf_<template-fp>_<seed-hash>`
name carries both, so either moving rebuilds), waits for the map to fill
(manual VACUUM marked nothing on this seed even minutes later; autovacuum's
first pass fills it — the loop polls every 10 s, budget 6 min, one-time per
fixture), and uses it read-only thereafter: steady-state re-runs take
~0.4 s. The `scripts/test-db.sh` reaper (`sanvi_t_*`/`sanvi_b_*`) never
touches it.

**Verification (scoped, this session).** `just test-perf`-equivalent
(`cargo nextest run -p sanvi-api --test perf` on the shared server):
16/16. Fails-before evidence: the before-plans above were measured on a
pre-migration scratch database (the pkey walks the gate forbids), and the
drop-index drill re-created that plan in place, the checker naming it; the
gate's first live runs also failed (6, then 4 named flips) on the
transient fresh-clone posture before the fixture waited for autovacuum —
recorded as the fixture-design lesson, not as a pass condition. Down/up
migration applied cleanly on a pristine clone; workspace suite
`scripts/test.sh --quiet` 1251/1251 (idor sweep, RLS matrix,
route/permission gates all green over the new migration), plus
`scripts/test.sh --quiet -p sanvi-persistence` 18/18 (migrator lifecycle)
scoped earlier; `cargo fmt --check` clean;
clippy `-D warnings` clean over sanvi-api and sanvi-persistence;
`scripts/check-layering.sh` OK; `scripts/check-openapi.sh` clean over all
14 specs (no contract change). No `.sqlx` changes (no macro queries
touched).

### Slice 11.2e record — 2026-10-06

Step 4 closed on the same Windows host. The slice's oracle and its fixes, in
that order.

**The gate.** New `apps/api/tests/perf/statement_counts.rs` (registered in
the perf target's `main.rs`; `just test-perf` runs the whole target, so no
recipe edit). The oracle is `pg_stat_statements`: every guarded use case runs
against its own cloned test database between two counter snapshots, and the
**diff** is the use case's statement profile — per-statement call counts plus
a total ceiling. Assertions are per-fragment ("the pricing page touches
`billing.plan_prices` exactly once"). Reintroducing a per-row loop turns the
exact-once into a multiple of the seeded row count, so the count test is the
falsifiable form of "no N+1 here". Design points: snapshots filter by `dbid`,
so a parallel nextest run cannot contaminate a window (every test owns a
database) and no global reset is used; the counter's own SELECT is excluded
by text; a vacuity guard fails an empty window; a missing preload fails
loudly naming the fix. Four no-database fixture tests pin the diff/matching
core, two of them `#[should_panic]` proofs that the assertions fire.

**Test-server prerequisite.** The shared test Postgres now preloads
`pg_stat_statements` (`testsupport::shared_postgres_request` and
`scripts/test-db.sh`), and `bootstrap` installs the extension on the template
under a momentary reopen — clones are born with the view, including templates
from older revisions (allowconn dance, idempotent). Containers started by
older code must be recreated once (`scripts/test-db.sh down && up` or
`docker rm -f sanvi_test_pg`); the failure mode is the snapshot error naming
exactly that, never a silent pass.

**The sweep.** All 15 context crates swept for per-element query loops
(multi-line regex for loop bodies containing `.await`, then every candidate
read by hand, adapters included). Clean: audit, media, notifications, system
— and localization/tenancy on all read paths. Seven read-model N+1s found and
fixed here:

1. **billing** `ListPublicPlans`/`ListPlans` — the 11.2c profiled N+1 (one
   prices query and one entitlements query per plan row; 1,803 calls each per
   601 requests). New port methods `list_active_for_plans` /
   `list_for_plans` (`plan_id = ANY($1)`, grouped into `BTreeMap`); the use
   cases issue one statement per child table regardless of catalog size.
2. **domains** `ListCustomDomains` — up to 200 per-domain certificate
   lookups per page to fill `cert_expires_at`. New
   `CertificateRepository::find_for_domains` (DISTINCT ON (domain_id) …
   `not_after DESC`, preserving the latest-cert semantics).
3. **identity** membership `list`/`list_with_emails` — per-member
   `role_ids_for` behind every member listing. New `roles_for_users` (one
   tenant-wide `membership_roles` query, grouped) — the shape
   `list_all_platform` already used.
4. **payments** `ExportPayments` — 3 queries per exported row (refund sum,
   dispute list, checkout fetch), repeated per page. Three per-page batches:
   `sum_succeeded_for_payments` (GROUP BY), `latest_for_payments`
   (DISTINCT ON (payment_id) … `updated_at DESC` — preserves the documented
   most-recent-dispute-wins rule), `find_by_ids` on checkouts.
5. **payments** `ListDisputes` — the deep-link chain resolved
   payment → checkout → connection per dispute (memoized, still up to 3
   queries per row). Now one batch per hop (`find_by_ids` on payments,
   checkouts, provider_connections); the live-connection fallback is
   untouched, and the guard's exactly-one connections query proves the chain
   resolved without it.
6. **platform_admin** tenant views — 3 COUNT queries per tenant per page.
   New `count_members_platform_batch` (identity),
   `count_active_batch` (access), `count_active_batch` (impersonation
   grants): three grouped statements per page.
7. **access** role hydration — `granting`/`list_all`/`assignable_to` fetched
   permissions per role; this compounded into `/me` through
   `SqlxRoleCatalog::permissions_for_roles`. `hydrate_many` batches all
   roles' permissions into one `ANY($1)` query; single-row `hydrate` stays
   for `find_by_id`/`find_by_key`.

**advertising** `list_creatives` resolved the feature gate per creative —
deduped to one check per distinct platform (the answer depends only on
(tenant, platform); the catalog is in-memory). In-process, not SQL, so no
statement guard applies; behavior is identical. **Left deliberately** (not
read models): the sweep's write-path loops (multi-row INSERTs, per-row
DELETEs on bounded job batches, fixed small catalogs) are recorded in the
sweep notes and stay as they are — this slice's bar is "no N+1 remains in any
read model".

**The guards.** Seven use-case guards (billing ×2, domains, identity,
payments, platform_admin, access), each seeding ≥3 rows through the context's
own repositories or audited raw SQL before the window opens, asserting exact
per-table counts plus a total ceiling — the fixture sizes are what make a
reintroduced loop a visible multiple rather than an off-by-one.

**Fails-before (the slice's drill).** The per-plan loop was deliberately
reintroduced in `ListPublicPlans`; `list_public_plans_issues_one_statement_
per_table` failed with `statements matching "FROM billing.plan_prices" must
fire exactly 1x per use case, saw 6x` (the migrated catalog's 3 plans + the
fixture's 3), the window dump showing the loop's 6+6 pattern. Reverted; the
batched form is what ships.

**Fixture lessons (for the next gate author).** Raw-SQL seeds run on the
**admin pool** — the migrator role is an RLS subject on
`identity.memberships`, `access.roles`, `payments.provider_connections`
(42501 on insert); the superuser is not. Real FKs to respect:
`membership_roles.role_id` → `access.roles`,
`memberships.user_id` → `identity.users`,
`checkouts.connection_id` → `provider_connections`,
`one_primary_per_tenant` (domains; seed role Alias for the 2nd/3rd row).

**Verification (this session).** `scripts/test.sh --quiet` **1262/1262**
(+11 = the new guards); the six context suites 416/416; advertising + api
342/342 (idor sweep, RLS matrix, bounded_collections, explain_assertions
green alongside); `cargo fmt --check` clean; clippy `--all-targets
-D warnings` clean over the nine changed crates; layering OK; openapi clean
over all 14 specs (no contract change); route-matrix regenerated — **one
fingerprint row** (`advertising/src/adapters/http/handlers.rs`), no route
rows; ga-checklist OK. Every new SQL statement is the runtime-checked
`query_as` form — no `.sqlx` cache changes.

### Slice 11.2f record — 2026-10-06

Step 5 closed on the same Windows host as 11.2a–e; the measurement record is
`benchmarks/pool-sizing-2026-10-06.md` (raw sampler log committed beside it;
two 3× probe artifacts committed as `benchmarks/<scenario>/pool-sz.json`).
The slice's numbers: **api ceiling 10 survives 3× the projected peak** (busy
≤ 5 of 10 at any sampled instant, idle ≥ 5, no acquire probe above the 50 ms
bucket across 744 probes, zero failures); **worker peak 4 connections**
across every shape, so its new ceiling is 8 (2× measured peak); **statement
ceiling in the heavy window 0.2 ms**, so the worker's new 60 s budget is a
runaway bound five orders of magnitude above anything measured, not a tuned
limit.

**The load-bearing change is the split.** The worker previously took
`[database]` whole — the production overlay shipped `max_connections = 50`
*per process*, i.e. 100 against a default-100 Postgres before any migrator,
superuser or session connection, an unexamined coincidence the 11.2a gauges
never measured. Now `[database]` is the request-facing (api) pool and the
worker applies a `[worker_pool]` overlay over it (`WorkerPoolConfig` in
`apps/worker`, Option fields merged before `validate`, so validation sees
the merged pair the way `build_pool` will): ceiling 8, statement timeout
60 000 ms (jobs are not request-shaped; the api keeps 10 000 under the 30 s
middleware), `application_name = "sanvi-worker"`. Prior values for every
changed setting are in the benchmark doc's decision table; the api's
production 50 remains a prior guess the lab cannot measure and is recorded
as exactly that.

**Per-process attribution.** `[database].application_name` (new, defaulted
`"sanvi"`) replaces the hardcoded form in `build_pool`, and base.toml sets
`sanvi-api` / `sanvi-worker` — `pg_stat_activity` now names the process
holding each connection, which is how this capture separated the pools
(arithmetic this time: total − api gauge; the worker has no HTTP scraper,
the pre-existing export posture) and how every later capture (11.2i's soak,
the capacity model) reads them directly. Four overlay unit tests in the
worker cover inherit-only, override-only-carried-keys, validate-on-merged,
and the distinct application name (empty override rejected).

**Pool saturation is alert-covered (TASK-020 registry).** Two rules in
`observability/alerts/rules.json`: `DbPoolAcquireWaitHigh` (ticket;
p95 probe wait > 100 ms sustained 5 min — 20× the measured healthy ceiling
of ≤ 5 ms, well under the 200 ms storefront budget, so it fires while there
is still time to act) and `DbPoolAcquireFailures` (page; any probe acquire
failure in 5 min — acquires failing means requests/jobs failed with them).
Exprs ride the existing `db_pool_*` instruments from 11.2a
(`histogram_quantile` over `db_pool_acquire_seconds_bucket`; `increase` on
`db_pool_acquire_failures_total`); `EXPORTED_METRICS` gained the four pool
metrics anchored to `pool_metrics.rs` in the alert-rules test (6/6 green,
including the fire-on-breach / silent-when-clean evaluation for both new
rules). New runbook `docs/runbooks/db-pool-saturation.md` carries both ids
(runbook-xref OK) and opens with the probe semantics — the acquire number is
a 1 Hz probe, not a hook — plus the assess/fix/verify loop (attribute by
`application_name`, distinguish a slow database from a held connection, fix
the query before touching a ceiling, record prior values).

**Defect found and fixed here (measurement-led, the only config-behaviour
change beyond the split).** The worker could not boot from the committed
configs: `[scheduler]` in base.toml never received the two TASK-017 (10.8)
keys `SchedulerConfig` requires (`advertising_budget_guardrails_interval_secs`,
`advertising_budget_rollover_interval_secs` — the struct has no per-field
serde defaults), so `WorkerConfig::validate` failed before a pool existed.
Every environment that did not spell those keys as env overrides was running
the api with no worker at all. Fixed by landing the struct's documented
defaults in base.toml (the file's convention for every other interval) —
and this is why the worker's side of the 11.2a pool capture was never taken.

**Verification (scoped, this session).** Measurement first: the seven
captured runs (five offered-rate + two 3× probes) finished 0 failed / 0
dropped, rc 0, and the sampler's raw log is committed. Then
`scripts/test.sh -p sanvi-persistence -p sanvi-api` (the slice's scoped
tier) plus the worker suite (`scripts/test.sh -p sanvi-worker` — four new
overlay tests) and the telemetry alert-rules gate; `cargo fmt --check`
clean; clippy `-D warnings` clean over the changed crates; runbook-xref and
ga-checklist OK. The lab release binaries were rebuilt from current main
before the capture (11.2d/e read-model changes included); migrations applied
on `sanvi_scale` (`migrate up` reports up to date) before any run.

### Slice 11.2g record — 2026-10-06

Step 6 closed on the same Windows host as 11.2a–f. The slice's two falsifiable
gates both live in `crates/platform/cache` and both pass:

**Single-flight stampede protection, proven at the required scale.**
`sanvi_cache::SingleFlight` (new `single_flight.rs`) is the per-key in-flight
map: `try_lead`/`wait` primitives plus `get_or_load`, the canonical cache-aside
loop (check → lead-or-wait → load once → fill → return; waiters exit through
the cache re-read; a failed load releases the flight and the next waiter leads
its own attempt — fail-closed, never a fabricated value). The gate test drives
500 concurrent misses on one key through `get_or_load` and asserts the loader
ran exactly once and every caller received the leader's value; supporting
tests cover lead-after-completion, a parked waiter woken by completion, and
wait-when-idle. Two defects the test itself caught during development: the
first draft dropped the leader's guard before awaiting the load (the flight
completed instantly and every caller loaded — exactly the stampede it exists
to prevent), and the closure shape that survives the `Send`-at-async-trait
boundary is boxed-future callbacks (`Fn() -> Pin<Box<dyn Future + Send + 'a>>`),
not `AsyncFn` bounds, whose HRTB `Send` proof rustc cannot make at the
`FeatureGate` port's async-trait boundary. Both findings are recorded here so
the next caller does not re-learn them.

**Wired into the two expensive shared-per-tenant keys** (the 11.2c profile's
hot paths): `ResolveThemeService::resolve_live` (the public storefront theme
read — one resolution per tenant+locale per expiry instead of one per
in-flight request; the DB path moved verbatim into `resolve_live_uncached`)
and `EntitlementResolver::resolve` (one resolve_from_db per tenant per 300 s
expiry; its in-flight set also guards the worker's budget-guardrails gate).
Both keep their existing invalidation semantics untouched — single-flight
sits between cache miss and database, it does not extend what is cached.

**Hit ratios per cache.** New `sanvi_cache::CacheMetrics`
(`cache_hits_total`/`cache_misses_total`/`cache_stampede_joins_total`,
`context`-labelled, registered on the process registry) wired into the
entitlement resolver (`context="access.entitlements"`, both apps) — the
entitlement cache previously had no hit-ratio instruments at all. Theming
already counted hits/misses (`ThemingMetrics`, 11.2c's capture used them) and
localization carries its own `hit_ratio()`; decisions, sessions, resolution
and domains host remain un-instrumented and are recorded as such: all four
are per-identity or pre-tenant lookups whose miss cost is one indexed
statement, not a fan-out.

**The tenant-key audit is a gate, not a review.**
`crates/platform/cache/tests/key_audit.rs` scans every `src/` tree under
`crates/` and `apps/` for `"sanvi:` key literals; each must either
interpolate `{tenant` in the format string or sit on a justified allow-list
(tenancy resolution and domains host: pre-tenant by definition; sessions:
token-scoped with the tenant in the payload; decisions: the tenant-scoped
resource id is embedded in the key; abuse/rl counters: request counts, never
tenant payload; scheduler leader: infra lock; `sanvi:*:` scan patterns: the
wildcard twins of tenant-shaped key families). A new tenant-less key fails
the test naming the file and the fix; the self-test plants a naked key shape
and asserts the named failure, and vacuity guards assert the scan reached the
known key sites (the 11.5g Windows-path-separator lesson — this scanner reads
bytes, not separators). Today's audit result: **every tenant-data cache
(entitlements, resolved themes, membership versions, decisions) carries a
tenant component**; the platform crate's `TenantCache` doc-claim that it is
"the only way tenant-scoped data reaches Redis" is false as written —
contexts own their adapters with hand-rolled keys, which is exactly why the
gate, not the helper, is the enforcement — and the doc now says so.

**TTLs are justified per entry, not inherited** (audit result, no change
needed): sessions ≤60 s (revocation staleness bound, index +60 s for the
logout webhook), decisions 1–30 s with a config validator that rejects
anything wider, entitlements 300 s (event-driven invalidation is primary;
TTL is the lost-event bound), resolved themes 24 h (publish/rollback
invalidate immediately cross-instance), resolution/domains host
config-driven with worker-side invalidation. Negative caching: the domains
host cache already caches unknown hosts in Redis (the attacker-influenced
per-request path), entitlement empty sets are cached as ordinary values, and
theming caches the default-theme fallback; tenancy slug resolution caches
positives only — its miss path is not hot (the per-request host lookup is
domains'), so a negative-caching change there is deliberately not taken.

**Verification (scoped, this session).** `cargo nextest run -p sanvi-cache`
11/11 including both gates;
`scripts/test.sh -p sanvi-access` 57/57, `-p sanvi-theming` 48/48,
`-p sanvi-api` 83/83 (perf gates included); `cargo check` clean for
sanvi-worker; `cargo fmt` clean; clippy `--all-targets` clean (zero
warnings) over the five changed crates. The platform crate gained a
`prometheus` dependency and a `futures-util` dev-dependency; access gained
`sanvi-cache`; api and worker gained `sanvi-cache` for the resolver wiring.

### Slice 11.2h record — 2026-10-06

Step 7 closed. The slice's numbers are in
`benchmarks/transport-2026-10-06.md`; the shape, in brief:

**Compression with a floor, not the stock middleware.**
`sanvi_http::SelectiveCompression` (new `compression.rs`) negotiates with
actix's own `AcceptEncoding::negotiate` (brotli > zstd > gzip > deflate at
equal q) and encodes with actix's own `Encoder` — but refuses what the
stock `Compress` accepts: bodies under `CompressionConfig::min_bytes`
(1024, `[server.compression]`, `enabled = false` is the rollback),
already-compressed content types (allow-list is textual only — the stock
predicate still encodes `application/zip` and audio), and any response
already carrying a non-`identity` `Content-Encoding`. Unknown types fail
closed. Refuse-everything clients get `406` + `Vary` without the handler
running; every encoded response carries `Vary: Accept-Encoding` (the stock
middleware sets it only on its 406 path). The floor is measured, not
chosen: the contract renders 320,559 B → ~51 KB gzip, while a 219 B
problem body saves 38 B — tens of bytes at the cost of a compression pass
on the hottest path.

**Keep-alive 5 s → 60 s.** actix's default is 5 s (verified in the
vendored `actix-http` source) — handshake churn on every idle gap.
`server.keep_alive_secs` (`0` disables), wired in `bind_without_run`;
60 sits at the ubiquitous balancer idle default. The production balancer's
actual timeout is the number this host cannot supply — recorded as tunable
config, not measurement.

**ETags.** `ConditionalEtag` (new `etag.rs`) stamps a strong validator
(`"sha256[..128]"` over the identity bytes) on successful GET/HEAD
responses and answers `304` on weak-comparison match (`*`, lists, `W/`);
streams, empties, non-2xx and already-tagged responses (theming,
localization — left authoritative) pass through. `/openapi.json` is
pre-rendered once with `Cache-Control: public, no-cache`: the 11.2c
profile's 25 ms rebuild disappears for 200s and revalidations alike; two
reads are byte-identical. `/metrics` gets a validator like everything
else but no cacheability — deliberately not cached, it is live per scrape.

**Payload trimming: audited, not cut.** `to_json()` is already compact
(verified in the vendored utoipa source); field removal would change
response bodies, which this task forbids. `PublicPlanView` and
`PrivacyNoticeView` are lean; `ResolvedTheme`'s dual `tokens`/`css_vars`
plus in-body `etag` are frontend-contract decisions deferred to TASK-026 /
frontend TASK-022 — named here so the next slice does not re-audit.

**Fails-before evidence.** The two first runs failed and are recorded:
`Some(0)` with a zero floor encoded an empty body (the decision now
requires non-empty — the `Encoder` agrees), and a hand-typed `Vary`
substring in the test never matched (the assertion now lowercases both
sides). The required floor proof: 1023 B passes through, 1024 B encodes
(`>=`, pinned); a 4 KiB PNG and a handler-encoded gzip pass through
byte-identical; br and gzip round-trip to the exact handler bytes.

**Verification (scoped, this session).** `cargo nextest run -p sanvi-http`
100/100 (11 transport wire tests + decision-matrix units included);
`sanvi-api` http_contract 18/18 (incl. the new pre-render/conditional test
with its above-the-floor assertion) + headers_snapshot 3/3; `cargo fmt`
clean; clippy `--all-targets -D warnings` clean over `sanvi-http` +
`sanvi-api`; layering OK; `openapi --check` clean over all 14 specs (no
contract change); route-matrix regenerated — fingerprint rows only
(`apps/api/src/lib.rs`, five `sanvi-http` rows incl. the two new modules),
no route rows; ga-checklist OK. New deps: `sha2` + `hex` (sanvi-http),
`flate2` + `brotli` workspace (dev-decoders for the round-trip tests;
both already in the lock). No `.sqlx` changes (no queries touched). The
size probe (`size_probe.rs`) was deleted after its single run — it wrote
the measured bytes and nothing else.

### Slice 11.2i record — 2026-10-07

Step 8's load runs and step 9's capacity model, on the same Windows host as
11.2a–h (20 cores/16 GB, **shared with unrelated co-tenant Docker stacks —
ragflow/Elasticsearch, a kind control plane, cmak — recorded because it is
the honest description of every number here**). Release binaries rebuilt
from current main before any capture (11.2b–h included; build stamp
`0.10.0`, commit `unknown`, the same unstamped-local shape as rc0).

**3× steady state (`benchmarks/load3x-2026-10-07.md`, artifacts
`<scenario>/x3.json`, sampler `load3x-2026-10-07/samples.txt`).** All five
shapes concurrently at 3× rates (690 iterations/s offered) for 300 s:
**every budget holds, every resource under 70 %** — storefront p95 9.3 ms
(4.7 % of the 200 ms budget), admin 9.8 ms, checkout 100 % success (lab
boundary), 252,007 requests, 0 failed / 0 dropped; api CPU max 13.1 % of
the machine (1.5 core-ms/request mean), RSS max 109 MB, pool busy ≤ 5 of
10, Redis 1.83 MB. Host CPU peaked 94 % — host-wide including the five k6
generators and co-tenants, not the API.

**10× spike (`benchmarks/spike10x-2026-10-07.md`, `<scenario>/x10.json`,
recovery artifacts `rec.1.json`/`rec.2.json`).** Two identical 2-minute
spikes at 2,300 iterations/s offered degraded **differently** — run A
through host/CPU contention (pool idle 8–10, host 76 %), run B through the
10-connection pool (idle → 1, ~1 s mean acquire waits, RSS → 769 MB,
host 37–46 %) — p95 6.4 s to 46 s, error rates ≤ 0.45 %, no restart, no
5xx storm. **Recovery immediate and complete both times**: offered-rate
storefront immediately after each spike ran p95 6.5 / 6.7 ms, 0 failed /
0 dropped, process start time unchanged. Two honesty notes recorded with
the artifacts: the harness's `rate × 4` VU cap client-starves the offer
once latency exceeds ~4 s (the server never received the full 10×), and
RSS does not return to ~100 MB post-burst — it plateaus at ~460 MB, which
is the number the soak watches and the capacity model budgets.

**perf-compare rc0 → rc.1 (offered-rate set, committed).** Two shapes
flagged (checkout_create p95 +41 %, webhook_flood p99 +26 %); the run was
**not** accepted as a regression and **not** waived on authority either —
seven same-binary repeats of the flagged shapes (rc.1b…rc.1g, committed)
show the flagged statistics are tail noise: checkout p50 flat at 3.8–4.4 ms
against rc0's 4.22 with p95 swinging 6.0–10.9 ms (rc0: 7.6) and p99
11.4–19.2 (rc0: 16.1); webhook p50 1.4–1.7 (rc0: 1.64), p99 4.2–5.6
(rc0: 4.42). Every rc0 statistic sits inside the same-binary repeat band;
the exception is explained here and in the artifacts rather than by
threshold loosening.

**24 h soak (`benchmarks/soak-2026-10-07/`).** Launched 2026-10-07 09:57
(+09:00) detached: five shapes concurrently at the offered rates for
86,400 s (`soak24` artifacts on completion) with a 30 s resource sampler
(RSS, threads, handles, per-process CPU, pool gauges, pg_stat_activity).
`soak.sh` / `sampler.sh` / `summarize.sh` are committed as the reproducible
harness; the pass conditions (flat memory from the ~460 MB plateau, no
connection growth, flat threads/handles — the per-task-count tokio-console
gap restated — 0 failed / 0 dropped) are in the README, and the verdict
appends to it when the run ends. **The soak completes ~09:57 on
2026-10-08; its verdict and the DoD lines that depend on it land in slice
11.2j's commit.**

**Capacity model (`docs/runbooks/capacity.md`).** Single-host, lab-measured,
labelled as such; every constant cites its committed artifact: CPU
1.5 core-ms/request (mean) with the requests/tenant/day arithmetic, memory
budget 512 MB/process (the 460 MB plateau + headroom), pool ceilings 10/8
with the production-50 prior guess still named as a guess, Redis
~12.5 kB/tenant, the degradation knee honestly bounded as "between 3× and
10×" with the two-bottleneck observation, and per-resource scaling
triggers wired to the existing alert ids. The document answers no alert and
is exempted in `runbook-xref-exempt.txt` with a reason (xref gate green).
The two numbers TASK-023 and TASK-026 take from this task — the shedding
ceiling and the capacity constants — are now measured and cited rather
than guessed.

**Verification (scoped, this session).** No Rust or SQL changed; the
scoped gates over the touched surface: `check-runbook-xref.sh` OK (after
the exemption line), ga-checklist OK, artifacts carry stats_version 2 +
machine profile + build stamp (`perf-compare` accepted rc.1 vs rc0 for
three shapes and refused nothing on provenance). Sampler defects found and
fixed during the runs (each recorded in the records above): first sample's
CPU delta is the process lifetime (drop row 1), a failed `/metrics` scrape
killed the first sampler mid-spike (`set -e` removed), CPU% divided by
nominal rather than elapsed interval (fixed via `$EPOCHREALTIME` deltas),
and the k6-presence stop condition needed to be opt-in. New in-flight
artifacts (`samples-soak24.txt`, `soak24.json` ×5) commit on completion.

### Slice 11.2j record — 2026-10-07 (task closed)

Step 8–9's soak verdict was the last open line, and the slice that was to
receive it became the slice that waived it. The full `## Verification`
block ran once, on the same Windows host as 11.2a–i, with the script
equivalents (`just` is not installed here — the task's own portable-form
convention): `cargo fmt --check` clean; `cargo clippy --workspace
--all-targets -- -D warnings` clean; `check-layering.sh` OK;
`scripts/test.sh` full workspace suite **1304/1304** (nextest, agent
profile); `check-openapi.sh` clean over all 14 specs; `cargo nextest run
-p sanvi-api --test perf` **27/27** (bounded_collections +
explain_assertions + statement_counts together, first time all three ran
in one pass); a fresh perf run tagged **rc.3** (five shapes × 60 s at the
offered rates, same machine profile and 0.10.0/`unknown` build stamp as
rc.1, all shapes under their k6 thresholds, artifacts committed as
`benchmarks/<scenario>/rc.3.json`); and `perf-compare rc.1 rc.3` **clean
on all five scenarios** — no latency metric past the +20 %/noise-floor
tolerance, no failed-rate or dropped-iteration move, same machine profile,
same stats_version, same build stamp, so no `NOTE` lines either. That
closes the block's `rc.1 → rc.3` edge: rc.1 (11.2i) was the
post-optimization state, rc.3 is the task's exit state, and nothing moved.

**The soak was waived by the task owner.** During this slice the running
attempt was found frozen 13 minutes in — the host had entered Modern
Standby on its 5-minute idle timeout while the load ran (every process
froze together; the API never restarted, its RSS drop was working-set
trim). Two relaunches followed: the first attempt's restart died to a
sampler defect this session introduced and fixed (the `tasklist //FI`
k6-liveness check only parses when MSYS path conversion is on; the
relaunch inherited `MSYS_NO_PATHCONV=1`, the check misread "no k6", the
sampler exited after two rows — the check is now the portable
`tasklist | grep` form, committed with the note), and the second was
cancelled at the owner's decision: **this is a developer machine, and a
24-hour soak is not wanted on it.** The waiver is recorded in
`benchmarks/soak-2026-10-07/README.md` with the full attempt history
(attempt 1's partial CSV preserved; idle-sleep was disabled for the
attempts, `standby-timeout-ac/dc` restored to the prior 300 s / 180 s
afterwards, verified by re-read). Step 8's soak verify line and the DoD
line are recorded as **waived on this host, not passed** — the flatness
evidence the next host owes is spelled out in the README's pass
conditions. Everything else step 8 required stands as landed in 11.2i.

**One harness defect fixed here (the slice's only code change).**
`perf-compare`'s default scenario enumeration globbed every directory
under `benchmarks/` — true when TASK-019 wrote it, wrong since 11.2c/11.2i
added `profiles/`, `load3x-*`, `spike10x-*`, `soak-*` and `pool-sizing-*`
record directories, which failed the gate as "no baseline artifact" noise.
The enumeration now scopes to directories holding a candidate artifact for
the candidate tag; a directory that holds the candidate but not the
baseline still fails (the missing-baseline rule is untouched), and the
self-test grew cases for both the skipped record directory and the
unbaselined scenario directory (`--self-test` OK, 30+ cases). This is a
gate-logic fix, behaviour-preserving for every real scenario.

**Definition of done:** every line ticked; the soak line carries the
waiver inline with its pointer. Status flipped to **done** here and in
[`../backlog.md`](../backlog.md) in this commit. TASK-023's shedding
ceiling and TASK-026's capacity constants remain the measured numbers from
`docs/runbooks/capacity.md`, exactly as 11.2i left them.
