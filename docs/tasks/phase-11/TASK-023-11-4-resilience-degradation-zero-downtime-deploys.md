# TASK-023: 11.4 Resilience, degradation & zero-downtime deploys

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1112, FR-1113, FR-1114, FR-1115, NFR-1105, NFR-1106
**Depends on:** TASK-021
**Created:** 2026-08-21

**Slice:** 11.4 — Resilience, graceful degradation & zero-downtime deploys
**Prerelease:** `v1.0.0-rc.5` · **Flags:** `platform.load_shedding`, `platform.degraded_mode`

## Context

Decide, in writing, what the system does when each dependency is slow, erroring, or down — and then
prove each answer **by causing it**. A degradation rule that has never been exercised is a comment.

The bar here is behavioural: during a drill, users see a designed experience and operators see an alert
they have a runbook for. Nothing silently retries forever, nothing double-charges, nothing drops a
conversion, and no tenant can starve another.

The shedding thresholds are **derived from TASK-021's measured ceiling, not guessed** — which is why
this task depends on it rather than running in parallel.

## What to do

- [ ] **Contract** — two changes, both spec-first because clients must handle them. **Load shedding**
      returns `503` with `Retry-After` and the `platform/overloaded` problem type above the concurrency
      ceiling, documented in the shared error envelope. **Degraded-mode signalling** — responses served
      from a degraded path carry a documented header so the frontend can say so honestly rather than
      pretending everything is fine. `503` is distinct from TASK-026's `429`: `429` means you asked too
      often, `503` means we are too busy. Conflating them makes client backoff wrong in both directions.
- [ ] **Dependency matrix** — for Postgres, Redis, Kratos, Keto, Stripe, DNS/ACME, Google Ads, Meta, and
      object storage: the behaviour when slow, when erroring, and when down; the timeout; the retry
      policy; the circuit-breaker thresholds; and the user-visible consequence. Committed as
      `docs/runbooks/dependency-matrix.md`, **one row per dependency per failure mode**.
- [ ] **Degradation rules implemented** — Redis down → serve from Postgres at reduced throughput, shed
      non-essential work; Keto down → deny by default for writes, allow cached reads for ≤ 30 s then
      deny; Stripe down → queue and retry under idempotency keys, never double-charge; ad platforms
      down → queue conversions **with their directive snapshot**, never drop; object storage down →
      serve placeholders, block uploads with an honest error.
- [ ] **Timeouts and circuit breakers everywhere** — no unbounded outbound call remains; a CI gate fails
      on an HTTP client constructed without a timeout.
- [ ] **Backpressure** — bounded queues throughout `crates/platform/messaging`, load shedding above the
      measured ceiling, and **per-tenant fairness** so one tenant's burst cannot consume the shared
      pool. Fairness is tested with a hot tenant from TASK-019's scale data — that tenant exists in the
      seed precisely for this test.
- [ ] **Worker resilience** — poison-message handling, dead-letter queues with an operator path back,
      and retry budgets so a permanently failing job stops rather than burning capacity forever.
- [ ] **Chaos drills, run as scheduled game days** — kill the primary DB, sever Redis, expire all
      sessions, replay a webhook flood, and blackhole one ad platform. Each drill has a hypothesis, a
      runner **who did not write the code**, a dashboard to read, and a findings list with owners.
- [ ] **Zero-downtime deploys** — rolling restart under load with zero dropped requests, proven with the
      k6 harness running through the deploy. Migrations documented and enforced as expand/contract in
      two phases, with a CI check that a migration touching a live column ships without a matching
      contract step only when explicitly annotated.

## Acceptance criteria

- [ ] The dependency matrix is committed and **every row has an automated test that injects that failure
      mode and asserts the documented behaviour** — this is the task's core deliverable, not an extra.
- [ ] A CI gate fails on an HTTP client constructed without a timeout, proven in the PR.
- [ ] The hot tenant saturating its share leaves other tenants inside their SLO.
- [ ] Load shedding engages at the measured ceiling, returns `503 + Retry-After`, recovers cleanly once
      load drops, and **never sheds a webhook or a payment callback**.
- [ ] A simulated Stripe outage produces no double charge; queued retries settle and idempotency keys
      are honoured.
- [ ] A simulated ad-platform outage queues conversions with their directive snapshot; none are dropped
      and none are uploaded without the snapshot.
- [ ] A rolling restart under k6 load produces zero non-2xx attributable to the deploy, across both a
      restart and a rollback.
- [ ] The expand/contract CI check fails on an unannotated migration touching a live column.
- [ ] Five chaos drills are run with a written findings list; every finding is closed or owned with a
      date.

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just test-dependency-matrix
just check-timeouts
just chaos-drill --all --dry-run
just perf
```

## Out of scope

`429` rate limiting and quotas (TASK-026 — the semantics must agree with this task's `503`, but the
enforcement is separate). Backup, restore, and rotation drills (TASK-025, which builds on the degraded
modes proven here). Frontend outage UX, retry, and offline handling (frontend TASK-023).

## Files likely touched

- `crates/platform/{messaging,http,cache}/**` (bounded queues, shedding, breakers, degraded header)
- every outbound adapter (`stripe`, `google_ads`, `meta`, `acme`, `keto`, `kratos`, object storage)
- `apps/worker/**` (poison handling, DLQ, retry budgets)
- `docs/runbooks/dependency-matrix.md`, plus a runbook per new alert
- `scripts/check-timeouts.*`, `scripts/check-migration-expand-contract.*`
- `tests/resilience/**` (one injection test per matrix row)

## Notes / gotchas

- Rollback: `platform.load_shedding` off removes shedding (back to unbounded acceptance, the pre-slice
  behaviour); `platform.degraded_mode` off makes degraded paths fail loudly instead of degrading.
  **Circuit breakers and timeouts are not flagged** — an unbounded call is never the safer default, and
  reverting one is a code revert with a reason in the PR.
- The queued-conversion rule is a privacy rule wearing a resilience costume: a conversion queued without
  its directive snapshot is an upload we cannot later justify. Queue the snapshot with it or drop the
  event.
- A drill run by the person who wrote the code tests the code they remember writing. Rotate the runner.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
