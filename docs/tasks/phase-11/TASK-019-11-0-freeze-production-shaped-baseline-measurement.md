# TASK-019: 11.0 Freeze, production-shaped baseline & measurement harness

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1101, FR-1102, FR-1103, FR-1104, NFR-1101
**Depends on:** phases 00-10
**Created:** 2026-08-21

**Slice:** 11.0 — Feature freeze, production-shaped baseline & the measurement harness
**Prerelease:** `v1.0.0-rc.1` · **Flag:** none — this slice ships tooling, not behaviour

## Context

Phase 11 is judged in numbers, so the numbers have to exist before the work does. At the end of this
task there is a frozen release branch, a staging environment carrying production-shaped data, a
harness that produces comparable measurements in CI, four weeks of field telemetry summarised into a
baseline, and both external engagements booked against a named build.

Nothing user-visible ships and nothing gets faster. That is the point: without a stored baseline every
later slice can only *claim* improvement, and two optimisations that cancel each other out look
exactly like two that worked.

**The three rules this task establishes for the rest of the phase:**

1. **Baseline first, tune second.** Every optimisation slice opens by recording the current number and
   closes by recording the new one, in the same harness, against the same data. A regression against
   the stored run blocks the merge, not the release.
2. **No new features.** The only exception in the whole phase is TASK-026's quota surfaces.
3. **Time-boxed by the acceptance list.** Hardening is an infinite well. `ga-checklist.md` is the exit
   condition; anything discovered beyond it gets an owner, a ticket, and a post-GA date.

## What to do

- [ ] **Contract** — one addition to `api/system.yaml`: `GET /api/v1/system/build` returning commit
      sha, semver, built-at, and environment. Unauthenticated, cacheable, no tenant data. The frontend
      error tracker, the RUM collector, and TASK-025's status page all read it, so a field measurement
      can always be attributed to an exact build. No `/api/v1` changes beyond this.
- [ ] **Freeze and branch policy** — cut `release/1.0`. Only phase 11 work merges; a feature PR needs a
      written exception recorded in the PR. Dead feature flags are **listed** now and removed in
      TASK-030 — removing them here would invalidate the baseline mid-phase.
- [ ] **Production-shaped data generator** — `apps/cli` gains `seed-scale`, deterministic from a seed
      argument, producing the distribution production will actually have rather than a uniform one: a
      long tail of small tenants, a middle band, and three deliberately hot tenants (one with a huge
      `audit.entries` history, one with heavy `advertising.metrics_daily`, one with high `payments`
      volume and many refunds); realistic per-context row counts; a locale mix skewed to include
      Japanese content; themes published at several revisions; several custom domains at each
      lifecycle state. It refuses to run unless the target database name matches the staging pattern.
- [ ] **Staging parity** — instance sizes, multi-AZ posture, connection limits, and CDN configuration
      recorded in `docs/runbooks/staging-parity.md` with the deltas from intended production called out
      explicitly. Flags set to their intended GA defaults, so the pen test and the a11y audit see the
      real product rather than a half-lit one.
- [ ] **Measurement harness** — `tests/load/` k6 scenarios (storefront read, admin write, checkout
      create, webhook flood, conversion upload) driven by `just perf`; every run writes a JSON artifact
      to `benchmarks/<scenario>/<rc-tag>.json`. `just perf-compare` diffs the last two runs and exits
      non-zero past a per-metric threshold. Wired into CI **reporting-only** for now.
- [ ] **Query and cache baseline** — snapshot `pg_stat_statements` top 50, index usage, table and index
      sizes, cache hit ratios, and pool saturation from four weeks of staging/beta into
      `benchmarks/baseline-<date>.md`. This is TASK-021's input and TASK-028's input.
- [ ] **SLO input data** — per-journey latency histograms and error ratios for the six journeys named
      in the implementation plan, over the same four weeks. The plan's targets are **proposals** until
      this data confirms them or forces a renegotiation, which is TASK-020's decision to make.
- [ ] **GA checklist scaffolding** — `docs/release/ga-checklist.md` assembled from the acceptance
      criteria of every phase 00–11, each line with an owner column. A CI job fails while any line has
      no owner. This is the document TASK-030 closes.
- [ ] **Book the external engagements** — the third-party penetration test (TASK-024) and the external
      accessibility audit (frontend TASK-027), both scheduled against the frozen staging build with
      scale data loaded, both with a remediation window before the GA date. Booking is a task with a
      date, not an intention.

## Acceptance criteria

- [ ] `release/1.0` is cut, the branch policy is enforced, and the exception process is documented.
- [ ] `seed-scale` is deterministic: the same seed produces identical row counts and checksums; a
      different seed produces a different but still valid distribution.
- [ ] `seed-scale` refuses a non-staging database name, and a test asserts the guard cannot be bypassed
      by environment variable alone.
- [ ] Staging carries scale data; row counts and the tenant distribution are recorded in the runbook.
- [ ] `benchmarks/` holds a committed baseline for backend load, DB, and frontend budgets, all tagged
      `rc.1`.
- [ ] `just perf-compare` fails on an injected 20 % latency regression and passes on run-to-run noise.
- [ ] `GET /api/v1/system/build` returns the same sha the frontend bundles report, asserted in an e2e.
- [ ] Four weeks of field telemetry are summarised per journey, with each SLO proposal annotated with
      its observed value.
- [ ] `docs/release/ga-checklist.md` exists with every line owned, and the CI owner gate is live and
      demonstrably fails on an unowned line.
- [ ] Pen test and accessibility audit are **booked with dates**, against a named build, with
      remediation windows on the calendar.
- [ ] No behaviour change: the full regression suite is green and no benchmark moved.

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just seed-scale --seed 1 --dry-run
just perf
just perf-compare rc.0 rc.1
```

## Out of scope

Any tuning at all — profiling and query work is TASK-021, frontend budgets become blocking in frontend
TASK-022. Removing dead flags (TASK-030). Committing to SLO *targets* (TASK-020 does that against this
data). Turning any gate from reporting-only to blocking.

## Files likely touched

- `api/system.yaml` (build probe)
- `apps/cli/src/commands/seed_scale.rs` and its distribution fixtures
- `tests/load/**` (k6 scenarios), `scripts/perf-compare.*`, `justfile` (`perf`, `perf-compare`, `seed-scale`)
- `benchmarks/**` (first committed artifacts)
- `docs/runbooks/staging-parity.md`, `docs/release/ga-checklist.md`
- CI workflow definitions (reporting-only harness jobs, owner gate)

## Notes / gotchas

- Rollback: nothing to unwind. No production behaviour changed, the seeder cannot touch production by
  construction, the harness is CI-only, and the build probe is additive and cacheable.
- The three hot tenants exist for specific later tests: per-tenant fairness (TASK-023), single-tenant
  restore (TASK-025), and vacuum/bloat measurement (TASK-028). Do not "simplify" the distribution.
- Booking the two external engagements is the single highest-leverage item here. Both are approval and
  scheduling queues measured in weeks; a phase that waits until the relevant slice to book them ships
  late for reasons that have nothing to do with engineering.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-019 done --note "<PR or commit>"`
