# Phase 11 — Hardening, Observability & GA (backend)

**Target version:** 1.0.0
**Depends on:** phases 00–10 for the GA-gated half; **nothing** for the gate half
**Unlocks:** general availability and a public API contract we are willing to be held to

**Sliced into thirteen tasks in [`../tasks/phase-11/README.md`](../tasks/phase-11/README.md)**, which
is authoritative for order, slicing, and each slice's exit criteria. This plan stays authoritative for
*what* and *why*. Requirements are numbered `FR-11xx` / `NFR-11xx` in
[`../requirements.md`](../requirements.md). Everything cut from this phase for want of a second person,
a vendor, infrastructure, or traffic is recorded in
[`../release/needs-humans.md`](../release/needs-humans.md) with what unblocks it.

## Goal

Turn a feature-complete system into an operable one: behaviour under stress that is decided in writing
and proven by injection, isolation that is verified rather than assumed, a public API contract that is
frozen and enforced by the build, and a measurement harness that catches a regression between two
release candidates.

## The one invariant

**Nothing is claimed that has not been measured, and nothing is trusted that has not been rehearsed.**

An SLO with no alert is a wish, a backup that has never been restored is a hypothesis, and a "faster"
endpoint with no before-number is a feeling. Every optimisation task opens by recording the current
number and closes by recording the new one, in the same harness, against the same data (NFR-1101).

The corollary, which is what actually shapes this rewrite: **an acceptance criterion that cannot be
falsified by running a command is not an acceptance criterion.** Criteria that depend on a vendor
report, a second person's signature, or four weeks of production traffic were removed from the task
graph rather than left in place to be quietly ticked.

## The phase splits in two

The original plan made every task depend on a feature freeze across phases 00–10. Backend phase 10 is
still in flight (TASK-016 uncommitted, TASK-017 and TASK-018 open), so under that dependency the whole
phase is blocked and none of it can start.

Most of the durable value here does not need feature completeness, because it is **enumerating gates**
rather than auditing a fixed surface. A CI check that fails when a use case has no span, or when a
route has no permission, does not care how many use cases and routes exist — and it is worth more the
earlier it lands, because everything phase 10 has left to write then arrives already covered.

**Gate half — startable today, independent of phase 10.**
CI coverage repair, the measurement harness, observability gates, and the safety gates.
Tasks: 031, 019, 020, 024.

**GA half — needs phase 10 merged.**
Performance, the SSR cache, resilience, DR, the API freeze, error shapes, data lifecycle, compliance
evidence, and the release itself.
Tasks: 021, 022, 023, 025, 026, 027, 028, 029, 030.

## Scope

**In**
- Latency budgets per journey, measured on a pinned profile; alerting rules and runbooks.
- Trace and log coverage enforced by CI rather than reviewed by eye.
- Performance: profiling, query and index review, N+1 removal, caching, load and soak on the harness.
- Resilience: a written dependency matrix with an injection test per row, backpressure, degradation.
- Recovery: a timed restore drill against the compose stack, `/health`, `/ready`, graceful shutdown.
- Security: threat-model refresh, generated route/permission matrix, RLS and IDOR sweeps, abuse
  controls, the scanner set completed and kept blocking.
- API stabilisation: `v1` freeze enforced by a diff gate, per-plan rate limits and quotas.
- Data lifecycle: partitioning, archival with a rehearsed restore, retention verification, offboarding.
- Compliance evidence generated from live configuration, and a completeness test over data providers.

**Out**
- New product features. The single exception is the quota and usage surface in TASK-026, because a
  limit users cannot see is a support ticket with extra steps (NFR-1102).
- Everything in [`../release/needs-humans.md`](../release/needs-humans.md).

## Latency budgets (lab, not SLOs)

These are measured by `just perf` against `seed-scale` data on a pinned machine profile. They are
**regression guards**, not service level objectives: there is no traffic, so there is no error budget
to burn. They become SLOs, with burn-rate alerting against them, when there are users (see
`needs-humans.md`).

| Journey | Metric | Budget |
|---|---|---|
| Storefront read (SSR API) | p95 latency | ≤ 200 ms |
| Admin API writes | p95 latency | ≤ 400 ms |
| Checkout create (tenant) | success ratio under load | ≥ 99.5 % excluding provider declines |
| Webhook processing | time to entitlement effect | p99 < 30 s |
| Conversion upload | success ratio per sweep | ≥ 99 % |
| Domain issuance | verified → live | p95 < 10 min after DNS propagation |

A run that regresses more than the per-metric threshold against the stored baseline fails
`just perf-compare` and blocks the merge.

## Work breakdown

### 1. CI coverage repair (TASK-031)

The gates that already exist do not cover what they appear to. `pr.yml` has two test jobs: the unit
job runs `--workspace --exclude sanvi-persistence --exclude sanvi-messaging --exclude
sanvi-tenancy`, and the integration job enumerates eight crates by hand. An earlier revision of this
section claimed six context crates (`sanvi-access`, `sanvi-advertising`, `sanvi-audit`,
`sanvi-identity`, `sanvi-platform-admin`, `sanvi-theming`) ship a `tests/` directory that no CI job
runs; measurement showed the unit job's `--workspace` selects their integration targets after all
— but only by an accident nobody could see, because nothing checked what the jobs actually cover,
and a job that runs no tests still passes.

Enumeration by hand is the defect. The integration job derives its crate list from the workspace, and
a crate with an integration target that CI does not run fails a check with a self-test proving it
fails on a dropped crate. The same task fixes the stale claims in `test-performance-baseline.md`,
adds the missing SAST scanner to the otherwise complete blocking set, and makes the Gatekeeper
exemption a detected prerequisite rather than a paragraph in `CONTRIBUTING.md` that silently
regresses.

### 2. Measurement harness and production-shaped data (TASK-019)

`seed-scale` in `apps/cli`: deterministic from a seed, producing the distribution production will
actually have rather than a uniform one — a long tail of small tenants, a middle band, and three
deliberately hot tenants (heavy `audit.entries`, heavy `advertising.metrics_daily`, high `payments`
volume with many refunds). The hot tenants exist for three specific later tests: per-tenant fairness
(TASK-023), single-tenant restore (TASK-025), and bloat measurement (TASK-028).

`tests/load/` k6 scenarios driven by `just perf`, each run writing a JSON artifact to
`benchmarks/<scenario>/<tag>.json`; `just perf-compare` diffs two runs and exits non-zero past a
per-metric threshold. `GET /api/v1/system/build` so a measurement can name the build that produced it.

### 3. Observability gates (TASK-020)

Trace coverage as a **committed matrix with a CI test that walks the registered use cases and fails on
an unspanned one** — the artifact, not the audit, is the deliverable. Every outbound call is a child
span with peer attributes. A log scanner runs the emitter over a fixture corpus containing every
PII-shaped value the platform holds, including a base64-wrapped token and a hashed identifier that is
still an identifier, and fails the build on a leak. Alert rules unit-tested against recorded metric
series. A bidirectional runbook cross-reference: an alert with no runbook fails, a runbook with no
alert fails.

### 4. Safety gates (TASK-024)

The security work that does not need an external tester. A generated route/permission matrix derived
from the OpenAPI documents plus handler metadata — every route with its permission, tenant scoping,
and rate-limit class — where a route appearing without one fails CI. The phase-01 RLS matrix re-run
across the full schema **including every child partition by name**, because RLS does not automatically
apply to a partition created after the policy, and that is the isolation gap most likely to be real.
A manual IDOR sweep across every ID-bearing path, query, header, and body field. Security headers
asserted per app in a snapshot test. The scanner set completed with SAST and kept blocking.

### 5. Performance (TASK-021)

Profile the top 20 endpoints chosen by (requests × latency) from the baseline. Query review against
`pg_stat_statements`: missing and duplicate indexes, sequential scans on tenant-scoped tables, and
**the measured cost of the RLS predicates** — the intuition that RLS is expensive is often right and
often wrong on the same schema, per table. `EXPLAIN (ANALYZE, BUFFERS)` assertions in the perf suite so
a plan regression fails CI. N+1 elimination with statement-count guards. Pool sizing derived from
measured concurrency. Cache review including **tenant scoping of every key, because a cache is a
bypass of RLS if its key forgets the tenant**. Cursor pagination everywhere; statement timeouts as
part of the contract.

### 6. SSR response cache (TASK-022)

Anonymous storefront responses keyed by `(tenant, locale, theme revision, device class)`, single-flight
on miss, invalidated by domain events rather than by an optimistic TTL. Authenticated responses are
never cached, asserted by a test rather than by a code comment. This is the one piece of the phase that
can serve a **wrong** response — another tenant's storefront — so it ships last within its slice, alone,
behind a flag.

### 7. Resilience (TASK-023)

A dependency matrix for Postgres, Redis, Kratos, Keto, Stripe, DNS/ACME, Google Ads, Meta, and object
storage: behaviour when slow, when erroring, and when down, with the timeout, retry policy, breaker
thresholds, and user-visible consequence. **Every row carries an automated injection test** — that is
the deliverable, not the document. Degradation rules implemented: Redis down serves from Postgres at
reduced throughput; Keto down denies writes and allows cached reads briefly; Stripe down queues under
idempotency keys and never double-charges; ad platforms down queue conversions **with their directive
snapshot**, which is a privacy rule wearing a resilience costume. Bounded queues, load shedding above
the measured ceiling, per-tenant fairness proven with a hot tenant. A CI gate failing on an HTTP client
constructed without a timeout. Expand/contract migrations enforced in CI; rolling restart under load
with zero dropped requests.

### 8. Recovery (TASK-025)

Scoped to what exists: a timed `pg_basebackup` plus WAL-replay restore drill against the compose
stack, a point-in-time restore verified across the three places a torn restore shows up first (outbox,
audit chain, payment records), and a single-tenant restore — the far more common real request — with a
dry-run mode, rehearsed against a hot tenant. Credential rotation end to end with a per-credential
duration and runbook, and **every old credential asserted to be rejected afterwards**, which is the
half that usually gets skipped. `/health` that returns with the database stopped, because liveness that
touches the database is the classic outage amplifier. `/ready` that reports states, never hostnames,
versions, or error strings.

### 9. API stabilisation (TASK-026)

**The last task in which breaking changes are cheap.** One consistency pass across all thirteen
documents — naming, pagination, filtering grammar, timestamp and money representation, the
problem-details type registry — then a freeze gate that diffs against a committed snapshot and fails on
a removed field, a narrowed enum, or a changed required-ness. Per-plan rate limits with `429` and the
`RateLimit-*` headers, kept explicitly distinct from TASK-023's `503 platform/overloaded`: conflating
them makes client backoff wrong in both directions. Quotas counted so they survive a restart, exposed
through `GET /api/v1/tenant/usage` — **reading the same counter enforcement reads**, because a UI
showing a different number from the one that blocks the request is worse than no UI.

### 10. Error shapes and ordering (TASK-027)

Small, cheap here, and impossible to do well downstream. Every validation error names the field it
belongs to, so a client can build an error summary that links to the offending input; the phase 08–10
wizards are the usual offenders because their multi-step payloads flatten on the way out. Every
collection endpoint specifies a deterministic ordering tie-broken by id, so keyboard position and
screen-reader context survive a refresh. Notification templates meet the same structure rules as the
apps, because an inaccessible email is the one failure a user cannot re-render around.

### 11. Data lifecycle (TASK-028)

Partitioning for `audit.entries`, `platform.outbox`, and the webhook event tables, from one job and one
registry covering all partitioned parents. Archival with a **rehearsed** restore path — archival
without one is deletion with extra steps. A read over an archived range returns an archived-range
response, never a silently partial result. Retention rules from phase 05 proven to *run* with a
per-class report. Tenant offboarding as export-then-purge with a residual scan that fails loudly, and a
purge report where **an empty retained list on a tenant with completed payments is a bug, not a clean
purge**.

### 12. Compliance evidence (TASK-029)

Evidence generated from live configuration, stamped with the build that produced it — a pack that
cannot name its build is not evidence, and hand-editing one destroys the only property that made it
worth generating. Art. 30-style records from the phase-05 data map, the sub-processor list with roles
that **differ per jurisdiction**, retention as published against retention as executed. A completeness
test enumerating registered `PersonalDataProvider`s and failing when one is missing from the access or
erasure path, because a context that quietly never joined is the most likely real defect. Erasure
propagation verified to reach archives, caches, and ad-platform suppression lists. The human drills and
counsel review are in `needs-humans.md`; the machinery they would exercise is built here.

### 13. GA release (TASK-030)

Close the checklist, re-run every gate, remove the scaffolding eleven phases accumulated (one flag per
commit, so a bad removal can be reverted without taking the good ones with it), rehearse migration up
and down on scale data, rehearse a revert under load, and tag `v1.0.0`. **This task adds nothing. Its
entire output is confidence, plus a tag.** It cannot tag until `sanvi-frontend` TASK-030 is done.

## Testing

- Full regression suite green, including every phase's acceptance criteria re-run as a release gate.
- Load, spike, and soak reports committed per release candidate; a regression against the stored run
  blocks the merge, not the release.
- Fault-injection suite green: one test per dependency-matrix row.
- Restore, PITR, single-tenant restore, and rotation drills completed with recorded durations.
- Isolation verified two ways that can be automated (RLS matrix including child partitions, IDOR
  sweep); the third way, an external test, is parked.

## Acceptance criteria

- [ ] Every context crate's tests run in CI, enforced by a check that fails on an unrun target.
- [ ] Every use case has a span and every route has a permission, both enforced by a CI gate that fails
      when a new one is added without it.
- [ ] The log PII scanner blocks with zero findings on a corpus containing every PII shape held.
- [ ] Every dependency-matrix row has an injection test asserting the documented behaviour.
- [ ] No unbounded outbound call remains; the timeout gate proves it.
- [ ] Cross-tenant isolation holds across the RLS matrix (child partitions by name) and the IDOR sweep.
- [ ] Load at 3× the harness's projected peak holds every latency budget on the pinned profile.
- [ ] A full restore, a PITR, and a single-tenant restore each complete in a timed drill with the
      duration committed; a neighbour tenant's rows are byte-identical before and after.
- [ ] Every credential rotates end to end, and every old credential is proven rejected.
- [ ] `/api/v1` is frozen, rate-limited, quota-enforced, and green on the replay suite.
- [ ] Rolling restart and revert under load drop zero requests.
- [ ] Offboarding exports, purges, and passes a residual scan; the purge report separates removed from
      lawfully retained.
- [ ] Evidence packs generate deterministically from live configuration and name their build.
- [ ] `docs/release/ga-checklist.md` is closed and `v1.0.0` is tagged.

## Risks

| Risk | Mitigation |
|---|---|
| Hardening becomes an open-ended sink | The acceptance list is the exit criterion; anything else becomes post-GA backlog with a date |
| Lab numbers get mistaken for production numbers | Every measurement records the machine profile and is labelled lab; the capacity model says what one host can tell you and stops there |
| Gate tasks land before the code they gate exists, so they enumerate an empty set | Each gate ships with a self-test proving it fails on a deliberately broken input — a gate that has never caught anything is a gate nobody has tested |
| Self-run security review by the author of the code | Every finding becomes a regression test, so the suite only grows; the external test stays in `needs-humans.md` as a named, unmitigated gap |
| Phase 10 lands after the gate tasks and skips them | The gates are CI-blocking, so phase 10's remaining work cannot merge without satisfying them |
| The freeze happens before phase 10's contract settles | TASK-026 is in the GA half and explicitly depends on phase 10 being merged |
