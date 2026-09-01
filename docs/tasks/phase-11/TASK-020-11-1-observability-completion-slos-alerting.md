# TASK-020: 11.1 Observability gates, latency budgets & alerting

**Phase:** 11 · **Status:** todo · **Size:** L
**Requirement(s):** FR-1105, FR-1106, FR-1107, NFR-1103, NFR-1111
**Depends on:** TASK-031, TASK-019
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01
**Flag:** `platform.tail_sampling`

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:executing-plans` or
> `superpowers:subagent-driven-development`. Every step carries a verification command. Do not tick a
> step whose command has not been run and passed.

## Goal

Close the gap between "we emit telemetry" and "we can answer a question at 3 a.m." — as **enforced
gates**, not as a one-time audit. Every use case has a span, every external call is a child span, no
log line leaks PII, every alert has a runbook and every runbook has an alert.

## Why this shape

This task blocks TASK-021 and TASK-022 for one reason: **you tune what you can see.** Profiling before
trace coverage is complete means the slow dependency is the one with no span, and it stays slow because
it stays invisible.

Everything here is written as a check that fails on a new violation rather than as a review that
passes once. That is what makes it startable before phase 10 finishes: the gates enumerate whatever
exists, and phase 10's remaining use cases arrive already covered.

The SLO targets from the original plan are now **lab latency budgets** (see the implementation plan).
There is no traffic, so there is no error budget to burn and no burn-rate alert to fire. The alert
rules are still written and still unit-tested against recorded series, because writing them is how you
find out that a metric you need does not exist.

## Constraints

- No `/api/v1` shape changes. Two cross-cutting conventions are fixed once in the shared schemas
  rather than per context: server-rendered documents and API responses carry the W3C `traceparent`,
  and every problem-details error carries a `trace_id`.
- Head sampling stays as the fallback when the collector is unavailable. **Losing traces must never
  cost requests.**
- No performance change of any kind in this task.

## File ownership map

- `crates/platform/telemetry/**` — span helpers, tail sampling, field standardisation
- every context's `application/**` — where a use case is missing a span
- `scripts/check-trace-coverage.sh`, `scripts/check-log-pii.sh`, `scripts/check-runbook-xref.sh`
- `observability/alerts/**` — alert rules as code
- `docs/runbooks/**` — one per alert, extending the existing seventeen
- `docs/slo/budgets.md` — the six journeys and their lab budgets
- the shared problem-details schema — `trace_id`

## Steps

### Step 1: Fix the two conventions in the shared schema

**Files:** Modify the shared problem-details schema, `crates/platform/http/**`; Test: contract test

**Do:** `trace_id` on every problem-details response; `traceparent` propagated on SSR documents and API
responses.

**Verify:** `just openapi` clean; a test asserts an error response carries a `trace_id` matching the
active span.

### Step 2: Write the trace-coverage check and let it fail

**Files:** Create `scripts/check-trace-coverage.sh`, `observability/trace-coverage.md`; Modify
`justfile`

**Do:** walk the registered use cases across the thirteen contexts and assert each emits a span named
`context.usecase` with tenant id, actor, and outcome as attributes. Emit the coverage matrix as a
committed artifact — **the matrix is the deliverable, not the claim**.

**Verify:** `just trace-coverage` → **fails**, listing the currently unspanned use cases.

### Step 3: Instrument until the check passes

**Files:** Modify every context's `application/**` where a span is missing

**Do:** add the missing spans. Then make every outbound call a child span with peer attributes:
Postgres, Redis, Kratos, Keto, Stripe, ACME/DNS, Google Ads, Meta, object storage.

**Verify:** `just trace-coverage` → passes. Then add a throwaway use case with no span in the same PR
and confirm the gate fails; remove it before merge.

### Step 4: Build the PII corpus, then the scanner

**Files:** Create `crates/platform/telemetry/tests/pii_corpus.rs`, `scripts/check-log-pii.sh`

**Do:** the corpus contains every PII shape the platform holds: emails, phone numbers, addresses,
tokens, card metadata, **a base64-wrapped token**, and **a hashed identifier that is still an
identifier**. The scanner runs the log emitter over it and fails the build on any leak.

**Verify:** `just log-pii-scan` catches every corpus value when redaction is deliberately removed from
one type, then reports zero findings on the clean tree.

### Step 5: Standardise fields and add tail sampling

**Files:** Modify `crates/platform/telemetry/**`; Modify `configs/**`

**Do:** consistent field names across contexts; levels tuned for production volume with a documented
cost estimate per level. Tail-based sampling on errors and latency outliers behind
`platform.tail_sampling`, with head sampling retained as the fallback.

**Verify:** a test asserts that with the collector unavailable, requests still complete and sampling
falls back to head.

### Step 6: Record the lab latency budgets

**Files:** Create `docs/slo/budgets.md`

**Do:** for each of the six journeys: the metric, its exact query, the budget, and the measurement
taken from TASK-019's harness. Label the whole document lab. Record explicitly that these become SLOs
with burn-rate alerting when there is traffic (`docs/release/needs-humans.md`).

**Verify:** every budget in the document has a measured value from a committed `benchmarks/` artifact.

### Step 7: Write alert rules and unit-test them

**Files:** Create `observability/alerts/**`; Create recorded metric series fixtures

**Do:** symptom-based rules only — no alert on CPU, on queue depth, or on any other cause that does not
by itself mean a user is having a bad time. Unit-test each against recorded series.

**Verify:** `just alert-rules-test` → a synthetic 10-minute outage fires; a single failed request does
not.

### Step 8: Close the runbook loop in both directions

**Files:** Create `scripts/check-runbook-xref.sh`; Modify `docs/runbooks/**`; Modify `justfile`

**Do:** one runbook per alert with detection, triage, mitigation, and escalation. The check fails on an
alert with no runbook **and** on a runbook with no alert.

**Verify:** `just runbook-xref` passes; deliberately orphan one rule and confirm it fails.

### Step 9: Execute every new runbook against the compose stack

**Files:** Modify `docs/runbooks/**`

**Do:** run each runbook's commands end to end against `docker-compose`. A runbook whose commands have
never been executed is a comment. Correct what does not work.

**Verify:** each runbook records the date its commands were last executed successfully.

## Definition of done

- [ ] `just trace-coverage` passes, and fails when a use case is added without a span.
- [ ] The coverage matrix is committed.
- [ ] `just log-pii-scan` blocks with zero findings on the corpus, and catches the base64-wrapped token
      and the hashed identifier when redaction is removed.
- [ ] Tail sampling is behind its flag; with the collector down, requests still complete.
- [ ] `docs/slo/budgets.md` records six lab budgets, each with a measured value and a query.
- [ ] `just alert-rules-test` distinguishes a 10-minute outage from a single failed request.
- [ ] `just runbook-xref` is green and fails on a deliberately orphaned rule.
- [ ] Every runbook's commands have been executed against the compose stack.
- [ ] A test asserts the `trace_id` in an error response equals the id in the corresponding span.

## Verification

```bash
just fmt
just clippy
just layering
just test
just openapi
just trace-coverage
just log-pii-scan
just runbook-xref
just alert-rules-test
```

## Out of scope

Any performance change (TASK-021). The SSR cache hit-ratio metric (TASK-022). Alerts for machinery that
does not exist yet — shedding (TASK-023), quotas (TASK-026), archival (TASK-028) — each ships its own
alerts and runbooks in its own slice.

## Parked

Dashboards per bounded context and per journey (there is no dashboard host), SLO targets confirmed
against observed data, and the error-budget policy that freezes feature merges — see
[`../../release/needs-humans.md`](../../release/needs-humans.md).

---
*On completion: satisfy every line in `## Definition of done`, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this file
and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note. The two
must never disagree.*
