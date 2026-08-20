# TASK-021: 11.2 Backend performance, load testing & capacity model

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1108, FR-1109, FR-1110, NFR-1101, NFR-1104
**Depends on:** TASK-020
**Created:** 2026-08-21

**Slice:** 11.2 — Backend performance, load testing & capacity model
**Prerelease:** `v1.0.0-rc.3` · **Flag:** none — every change is behaviour-preserving

## Context

Make the API fast enough that the SLOs committed in TASK-020 hold at three times projected peak with
headroom to spare, and know — with a written model rather than a hunch — what it costs to serve the
next thousand tenants.

**Every change in this task is behaviour-preserving. If a response body changes, it belongs in another
task.**

This task produces two numbers other slices depend on and cannot derive themselves: the measured
concurrency ceiling that TASK-023's load shedding thresholds come from, and the capacity model that
TASK-026's per-plan rate limits come from. Set either by guess and the limits are decorative (too high)
or an outage of our own making (too low).

## What to do

- [ ] **Contract** — no `/api/v1` shape changes, but two contract-adjacent rules are enforced
      repository-wide and may require spec edits where a list endpoint is currently unbounded:
      **cursor pagination everywhere** (any collection endpoint without a bounded page size is a load
      test waiting to be run by a customer; offset pagination on large tables is replaced), and
      **statement timeouts are part of the contract** (a request that cannot complete inside its budget
      returns a problem-details error, not a held connection).
- [ ] **Profile the top 20 endpoints** — chosen from TASK-019's baseline by (requests × latency), not by
      opinion. `cargo flamegraph` for CPU, `tokio-console` for task stalls and blocked executors.
      Findings recorded per endpoint **with a before number**.
- [ ] **Query review** — `pg_stat_statements` top offenders from the baseline: missing and duplicate
      indexes, sequential scans on tenant-scoped tables, and the cost the RLS predicates add. Measure
      it — a policy that forces a scan is the single most expensive thing in a multi-tenant schema.
      `EXPLAIN (ANALYZE, BUFFERS)` assertions land in the perf test suite so a plan regression fails CI
      rather than surfacing as a page.
- [ ] **N+1 elimination** — sweep the read models across all thirteen contexts; introduce dedicated
      query-side projections where a join-per-row cannot be batched away. Repository-level tests count
      statements per use case and fail above the expected count.
- [ ] **Connection pool sizing** — derived from measured concurrency under the k6 steady-state run, not
      guessed; separate ceilings for `apps/api` and `apps/worker`, statement timeouts per pool, and
      pool saturation alerting wired to TASK-020's dashboards.
- [ ] **Cache review** (`crates/platform/cache`) — hit ratios per cache, single-flight stampede
      protection on every expensive key, negative caching where a miss is common and cheap to be wrong
      about briefly, and TTLs justified per entry rather than inherited. **Cache keys audited for tenant
      scoping — a cache is a bypass of RLS if its key forgets the tenant.**
- [ ] **Transport tuning** (`crates/platform/http`) — response compression with a size floor, keep-alive
      tuning, payload trimming (fields nobody reads are cost), and `ETag`/conditional requests where the
      storefront read path can use them.
- [ ] **Load, spike and soak** — TASK-019's scenarios run for real: steady state at 3× projected peak,
      spike to 10× for two minutes, and a 24-hour soak watching RSS, connection counts, and task counts
      for leaks. Results committed as `rc.3` benchmarks and compared against `rc.1`.
- [ ] **Capacity model** — a written model mapping requests/tenant/day to CPU, memory, DB IOPS, and
      Redis memory with the constants measured here, plus a documented scaling trigger per resource
      ("at X, add Y"). Published in `docs/runbooks/capacity.md` and referenced by TASK-025's on-call
      material.

## Acceptance criteria

- [ ] Top 20 endpoints profiled with before/after numbers committed.
- [ ] Every identified missing index added, every duplicate removed, and RLS predicate cost measured and
      recorded per table.
- [ ] A plan flipping to a sequential scan on any profiled endpoint fails CI.
- [ ] No N+1 remains in any read model; a statement-count test guards each one.
- [ ] Pool sizes and statement timeouts are derived from measurement, documented, and alerting.
- [ ] Cache stampede protection holds under 500 concurrent misses on one key, and a cache key without a
      tenant component fails a unit test by construction.
- [ ] Load at 3× peak holds every TASK-020 latency SLO with < 70 % utilisation on every resource.
- [ ] Spike at 10× for two minutes degrades within the error budget and recovers without a restart.
- [ ] A 24-hour soak shows flat memory, no connection growth, and no unbounded task accumulation.
- [ ] Cursor pagination on every collection endpoint; no unbounded list remains in any of the thirteen
      specs.
- [ ] `docs/runbooks/capacity.md` is published with measured constants and per-resource scaling triggers.
- [ ] `just perf-compare rc.1 rc.3` passes on every scenario, or the exception is explained in the PR.

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
(TASK-026), the SSR response cache (TASK-022), partitioning, archival, and vacuum tuning (TASK-028).
Frontend bundle work (frontend TASK-022).

## Files likely touched

- `crates/platform/{cache,http,persistence}/**` (pools, TTLs, single-flight, compression, ETag)
- every context's read models and query-side projections
- `migrations/**` (index additions and removals)
- `tests/perf/**` (`EXPLAIN` assertions, statement-count guards)
- `benchmarks/**` (`rc.3` artifacts), `docs/runbooks/capacity.md`
- the thirteen `api/*.yaml` documents where a list endpoint is still unbounded

## Notes / gotchas

- Rollback is **per-commit**, not per-slice: each optimisation is independently revertable and
  behaviour-preserving. The two changes carrying real risk are pool resizing and cache TTL changes —
  both are configuration, both ship with the previous value recorded in the PR description, and both
  are watched for one full traffic cycle on staging before the next slice starts.
- The frontend has two obligations against this task even though it owns none of the work: re-run its
  harness after these changes land (SSR latency feeds LCP, and a 50 ms server improvement is a real
  field improvement worth recording), and flag any endpoint it calls in a loop. An N+1 is often a client
  pattern, and that fix belongs in frontend TASK-022's prefetch and batching work rather than in a
  projection nobody needed. That is frontend TASK-021.
- Measure the RLS predicate cost before optimising around it. The intuition that RLS is expensive is
  often right and often wrong on the same schema, per table.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-021 done --note "<PR or commit>"`
