# TASK-021: 11.2 Backend performance, load testing & capacity

**Phase:** 11 · **Status:** todo · **Size:** L
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
