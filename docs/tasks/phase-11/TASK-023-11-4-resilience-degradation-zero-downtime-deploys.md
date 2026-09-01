# TASK-023: 11.4 Resilience, degradation & zero-downtime restarts

**Phase:** 11 · **Status:** todo · **Size:** L
**Requirement(s):** FR-1112, FR-1113, FR-1114, FR-1115, NFR-1105, NFR-1106
**Depends on:** TASK-021, **phase 10 merged**
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flags:** `platform.load_shedding`, `platform.degraded_mode`

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Decide in writing what the system does when each dependency is slow, erroring, or down — and prove each
answer **by causing it**. A degradation rule that has never been exercised is a comment.

## Why this shape

The original task ran the failure injection as scheduled game days with rotating human runners. There
is one person, so the ceremony is replaced by the thing the ceremony was protecting: **one automated
injection test per dependency-matrix row, run in CI.** That is the task's core deliverable, not an
extra alongside the document.

The shedding thresholds are derived from TASK-021's measured ceiling, not guessed, which is why this
task depends on it rather than running in parallel.

## Constraints

- `503` and `429` are distinct and must be decided together with TASK-026: `429` means the caller asked
  too often, `503 platform/overloaded` means the platform is too busy. Conflating them makes client
  backoff wrong in both directions.
- **Circuit breakers and timeouts are not flagged.** An unbounded call is never the safer default;
  reverting one is a code revert with a reason in the PR.
- Load shedding must **never** shed a webhook or a payment callback.
- A conversion queued without its directive snapshot is an upload we cannot later justify. **Queue the
  snapshot with the event or drop the event.** This is a privacy rule wearing a resilience costume.

## File ownership map

- `crates/platform/{messaging,http,cache}/**` — bounded queues, shedding, breakers, degraded header
- every outbound adapter — `stripe`, `google_ads`, `meta`, `acme`, `keto`, `kratos`, object storage
- `apps/worker/**` — poison handling, dead-letter queues, retry budgets
- `docs/runbooks/dependency-matrix.md` and a runbook per new alert
- `scripts/check-timeouts.sh`, `scripts/check-migration-expand-contract.sh`
- `tests/resilience/**` — one injection test per matrix row

## Steps

### Step 1: Specify `503` and the degraded header, contract first

**Files:** Modify the shared error envelope, `api/*.yaml`

**Do:** load shedding returns `503` with `Retry-After` and the `platform/overloaded` problem type above
the concurrency ceiling. Responses served from a degraded path carry a documented header so the
frontend can say so honestly rather than pretending everything is fine.

**Verify:** `just openapi` clean; the problem-type registry contains `platform/overloaded` and nothing
else claims that meaning.

### Step 2: Write the dependency matrix as the test manifest

**Files:** Create `docs/runbooks/dependency-matrix.md`

**Do:** one row per dependency per failure mode, for Postgres, Redis, Kratos, Keto, Stripe, DNS/ACME,
Google Ads, Meta, and object storage: behaviour when slow, when erroring, when down; the timeout; the
retry policy; the breaker thresholds; the user-visible consequence. Each row carries the path of the
test that proves it.

**Verify:** a check asserts every row names a test file that exists.

### Step 3: Write one injection test per row, and let them fail

**Files:** Create `tests/resilience/**`

**Do:** each test injects that failure mode and asserts the documented behaviour.

**Verify:** the suite compiles and fails on the rows whose behaviour is not yet implemented.

### Step 4: Implement the degradation rules until the suite passes

**Files:** Modify every outbound adapter and `crates/platform/**`

**Do:** Redis down → serve from Postgres at reduced throughput, shed non-essential work. Keto down →
deny by default for writes, allow cached reads for ≤ 30 s then deny. Stripe down → queue and retry
under idempotency keys, never double-charge. Ad platforms down → queue conversions **with their
directive snapshot**, never drop. Object storage down → serve placeholders, block uploads with an
honest error.

**Verify:** `just test-dependency-matrix` → green on every row.

### Step 5: Gate on timeouts

**Files:** Create `scripts/check-timeouts.sh`; Modify `justfile`, `.github/workflows/pr.yml`

**Do:** fail the build on an HTTP client constructed without a timeout. Add circuit breakers wherever
the matrix calls for one.

**Verify:** `just check-timeouts` fails on a deliberately added timeout-less client, then passes.

### Step 6: Add backpressure and per-tenant fairness

**Files:** Modify `crates/platform/messaging/**`, `crates/platform/http/**`

**Do:** bounded queues throughout; load shedding above the ceiling TASK-021 measured; per-tenant
fairness so one tenant's burst cannot consume the shared pool.

**Verify:** using a hot tenant from `seed-scale` — **which exists in the seed precisely for this test**
— saturate its share and assert other tenants stay inside their budgets. Assert shedding never sheds a
webhook or a payment callback.

### Step 7: Harden the worker

**Files:** Modify `apps/worker/**`

**Do:** poison-message handling, dead-letter queues with an operator path back, retry budgets so a
permanently failing job stops rather than burning capacity forever.

**Verify:** a poison message lands in the DLQ and is replayable; a permanently failing job stops at its
budget.

### Step 8: Enforce expand/contract migrations and prove a zero-downtime restart

**Files:** Create `scripts/check-migration-expand-contract.sh`; Modify `docker-compose.staging.yml`

**Do:** a migration touching a live column fails CI unless it ships with a matching contract step or an
explicit annotation. Rolling restart under k6 load in the compose stack.

**Verify:**
- The check fails on an unannotated migration touching a live column.
- A rolling restart under load produces zero non-2xx attributable to the deploy, across both a restart
  and a revert to the previous image.

## Definition of done

- [ ] Every dependency-matrix row has a passing injection test asserting the documented behaviour.
- [ ] `just check-timeouts` blocks a timeout-less client, proven in the PR.
- [ ] A hot tenant saturating its share leaves other tenants inside their budgets.
- [ ] Shedding engages at the measured ceiling, returns `503 + Retry-After`, recovers cleanly, and
      **never sheds a webhook or a payment callback**.
- [ ] A simulated Stripe outage produces no double charge; queued retries settle under idempotency keys.
- [ ] A simulated ad-platform outage queues conversions with their directive snapshot; none are dropped
      and none upload without it.
- [ ] Poison messages reach the DLQ and are replayable; retry budgets terminate a failing job.
- [ ] The expand/contract check fails on an unannotated migration touching a live column.
- [ ] Rolling restart and revert under load drop zero requests.

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just test-dependency-matrix
just check-timeouts
just check-migration-expand-contract
just perf
```

## Out of scope

`429` rate limiting and quotas (TASK-026 — the semantics must agree with this task's `503`, but the
enforcement is separate). Backup, restore, and rotation drills (TASK-025). Frontend outage UX
(frontend TASK-023).

## Parked

Chaos game days with a rotating runner — the automated injection suite replaces the coverage; the
independent-runner property is recorded as lost in
[`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
