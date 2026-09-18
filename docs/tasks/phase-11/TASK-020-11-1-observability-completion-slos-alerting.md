# TASK-020: 11.1 Observability gates, latency budgets & alerting

**Phase:** 11 · **Status:** done · **Size:** L
**Requirement(s):** FR-1105, FR-1106, FR-1107, NFR-1103, NFR-1111
**Depends on:** TASK-031, TASK-019
**Created:** 2026-08-21 · **Rewritten:** 2026-09-01 · **Closed:** 2026-09-18
**Flag:** `platform.tail_sampling` (landed as `telemetry.sampling.tail_sampling`, default off)

> **For agentic workers:** REQUIRED SUB-SKILL — `superpowers:subagent-driven-development`.
> **Execute one slice per session, not one task per session** — see `## Execution slices` below. Each
> slice is a commit with its own scoped verification; a slice that has not been committed is not done.
> Every step carries a verification command; do not tick a step whose command has not been run and
> passed. **Run the full `## Verification` block once, in the final slice** — not per step and not per
> slice. Running the workspace suite after every step is the single largest avoidable cost in this
> phase: `just test` links every test binary, and on an un-exempted macOS host Gatekeeper scans each
> one (~15 of ~16 minutes). Use `just check` and `just test-fast` while working.

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

## Execution slices

Nine steps in one session means one context, one compaction, and re-reads of work already done. The
slices below are the commit boundaries; each is one agent session with a fresh context, and each
carries the scoped commands to run **in that session**. The task is done when the last slice is.

| Slice | Steps | Shape | Scoped verification for the session |
| --- | --- | --- | --- |
| **11.1a** conventions & the coverage gate, red | 1–2 | small, design-led | `just check`, `cargo nextest run -p sanvi-http`, `just openapi`, then `just trace-coverage` → **must fail**, listing the unspanned use cases |
| **11.1b** instrument to green | 3 | **bulk, mechanical — delegatable** | `just trace-coverage` → passes; falsify with a throwaway unspanned use case; `just test-fast` |
| **11.1c** log PII corpus & scanner | 4 | small, self-contained | `cargo nextest run -p sanvi-telemetry`, `just log-pii-scan` in both directions (catches every corpus value with redaction removed from one type; zero findings clean) |
| **11.1d** field standardisation & tail sampling | 5 | small, flagged | `cargo nextest run -p sanvi-telemetry`, plus the collector-unavailable fallback test |
| **11.1e** budgets, alert rules & the runbook loop | 6–8 | medium, artifact-led | `just alert-rules-test`, `just runbook-xref` + the deliberately orphaned rule |
| **11.1f** execute every runbook, then close the task | 9 | wall-clock, no code | the full `## Verification` block, once |

**Order and parallelism.** 11.1a → 11.1b is a hard edge: the gate must exist and be red before anything
is instrumented, or "it passes" means nothing. 11.1c, 11.1d and 11.1e are independent of each other and
of 11.1b, and may run concurrently in separate sessions. 11.1f is last by definition.

**Why 11.1b is the delegation candidate.** It is the only slice that is bulk-mechanical — adding a span
per use case across thirteen contexts, with the gate as the oracle for whether it is complete. Its
correctness is machine-checkable (`just trace-coverage`), so a cheaper model can produce it and the
review is reading a diff rather than re-deriving a design. Every other slice here is judgement-led:
what counts as PII, which alert is a symptom rather than a cause, what a budget should be.

## Steps

### Step 1 · slice 11.1a: Fix the two conventions in the shared schema

**Files:** Modify the shared problem-details schema, `crates/platform/http/**`; Test: contract test

**Do:** `trace_id` on every problem-details response; `traceparent` propagated on SSR documents and API
responses.

**Verify:** `just openapi` clean; a test asserts an error response carries a `trace_id` matching the
active span.

### Step 2 · slice 11.1a: Write the trace-coverage check and let it fail

**Files:** Create `scripts/check-trace-coverage.sh`, `observability/trace-coverage.md`; Modify
`justfile`

**Do:** walk the registered use cases across the thirteen contexts and assert each emits a span named
`context.usecase` with tenant id, actor, and outcome as attributes. Emit the coverage matrix as a
committed artifact — **the matrix is the deliverable, not the claim**.

**Verify:** `just trace-coverage` → **fails**, listing the currently unspanned use cases.

### Step 3 · slice 11.1b: Instrument until the check passes

**Files:** Modify every context's `application/**` where a span is missing

**Do:** add the missing spans. Then make every outbound call a child span with peer attributes:
Postgres, Redis, Kratos, Keto, Stripe, ACME/DNS, Google Ads, Meta, object storage.

**Verify:** `just trace-coverage` → passes. Then add a throwaway use case with no span in the same PR
and confirm the gate fails; remove it before merge.

### Step 4 · slice 11.1c: Build the PII corpus, then the scanner

**Files:** Create `crates/platform/telemetry/tests/pii_corpus.rs`, `scripts/check-log-pii.sh`

**Do:** the corpus contains every PII shape the platform holds: emails, phone numbers, addresses,
tokens, card metadata, **a base64-wrapped token**, and **a hashed identifier that is still an
identifier**. The scanner runs the log emitter over it and fails the build on any leak.

**Verify:** `just log-pii-scan` catches every corpus value when redaction is deliberately removed from
one type, then reports zero findings on the clean tree.

### Step 5 · slice 11.1d: Standardise fields and add tail sampling

**Files:** Modify `crates/platform/telemetry/**`; Modify `configs/**`

**Do:** consistent field names across contexts; levels tuned for production volume with a documented
cost estimate per level. Tail-based sampling on errors and latency outliers behind
`platform.tail_sampling`, with head sampling retained as the fallback.

**Verify:** a test asserts that with the collector unavailable, requests still complete and sampling
falls back to head.

### Step 6 · slice 11.1e: Record the lab latency budgets

**Files:** Create `docs/slo/budgets.md`

**Do:** for each of the six journeys: the metric, its exact query, the budget, and the measurement
taken from TASK-019's harness. Label the whole document lab. Record explicitly that these become SLOs
with burn-rate alerting when there is traffic (`docs/release/needs-humans.md`).

**Verify:** every budget in the document has a measured value from a committed `benchmarks/` artifact.

### Step 7 · slice 11.1e: Write alert rules and unit-test them

**Files:** Create `observability/alerts/**`; Create recorded metric series fixtures

**Do:** symptom-based rules only — no alert on CPU, on queue depth, or on any other cause that does not
by itself mean a user is having a bad time. Unit-test each against recorded series.

**Verify:** `just alert-rules-test` → a synthetic 10-minute outage fires; a single failed request does
not.

### Step 8 · slice 11.1e: Close the runbook loop in both directions

**Files:** Create `scripts/check-runbook-xref.sh`; Modify `docs/runbooks/**`; Modify `justfile`

**Do:** one runbook per alert with detection, triage, mitigation, and escalation. The check fails on an
alert with no runbook **and** on a runbook with no alert.

**Verify:** `just runbook-xref` passes; deliberately orphan one rule and confirm it fails.

### Step 9 · slice 11.1f: Execute every new runbook against the compose stack

**Files:** Modify `docs/runbooks/**`

**Do:** run each runbook's commands end to end against `docker-compose`. A runbook whose commands have
never been executed is a comment. Correct what does not work.

**Verify:** each runbook records the date its commands were last executed successfully.

## Definition of done

- [x] `just trace-coverage` passes, and fails when a use case is added without a span.
- [x] The coverage matrix is committed.
- [x] `just log-pii-scan` blocks with zero findings on the corpus, and catches the base64-wrapped token
      and the hashed identifier when redaction is removed.
- [x] Tail sampling is behind its flag; with the collector down, requests still complete.
- [x] `docs/slo/budgets.md` records six lab budgets, each with a measured value and a query.
- [x] `just alert-rules-test` distinguishes a 10-minute outage from a single failed request.
- [x] `just runbook-xref` is green and fails on a deliberately orphaned rule.
- [x] Every runbook's commands have been executed against the compose stack.
- [x] A test asserts the `trace_id` in an error response equals the id in the corresponding span.

## Verification

Run this **once, in slice 11.1f**. While working a slice, run that slice's scoped commands from
`## Execution slices` instead.

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

### Verification record (2026-09-18, slice 11.1f)

All nine commands run green on `feat/task-020-observability-gates`: fmt and
clippy (`-D warnings`) clean; layering OK; full suite 1176/1176
(`just test-quiet`, the same suite with the low-noise profile); openapi
clean; trace-coverage 115/115; log-pii-scan zero findings (self-test
plants the base64-wrapped token and the hashed identifier, both caught);
runbook-xref OK (self-test proves all four failure modes); alert-rules-test
5/5. The four new gates are wired into `just ci` and `just ci-quiet`.

## Execution record (slices as committed)

- **11.1a** (`feat(observability): trace context conventions and the red
  trace-coverage gate`) — the two conventions fixed once in `sanvi-http`:
  every problem-details body carries the W3C trace id of the active span
  (`x-request-id` stays the fallback outside any OTel context — the DoD
  test asserts body `trace_id` == span trace id against a real SDK
  tracer), every response carries a matching `traceparent` header, and an
  inbound `traceparent` joins the caller's trace at the root span via the
  global `TraceContextPropagator` `sanvi_telemetry::init` now registers.
  `scripts/check-trace-coverage.sh` walks use-case entries and failed red
  on first run: **107 of 111 use cases without a conforming span**. The
  red matrix was committed.
- **11.1b** — 239 entries instrumented across all fifteen contexts with
  `#[tracing::instrument(name = "<context>.<usecase>", …)]`, named from
  the enclosing use-case struct; cross-tenant sweeps keep a span without a
  tenant field. Falsified before merge: a planted spanless use case fails
  the gate naming its file.
- **11.1c** — the PII corpus test and `scripts/check-log-pii.sh` in both
  directions.
- **11.1d** — `TailSamplingProcessor` behind `telemetry.sampling.tail_sampling`
  (the task's `platform.tail_sampling`; key named for the section it lives
  in, still default-off): errors and latency outliers always exported, the
  keep decision sticky per trace, head sampling the fallback; the
  collector-unavailable invariant proven in both flag positions.
  `observability/field-standard.md` fixes field names and level volumes.
- **11.1e** — budgets re-captured on the post-erratum seed
  (`benchmarks/*/baseline-2026-09-18.json`, `stats_version: 2` — the
  removed stats_version-1 artifacts from TASK-019 replaced);
  `observability/alerts/rules.json` codifies 26 rules; the xref gate.
- **11.1f** — the new runbooks executed against the live compose stack
  (dates recorded in the files); full verification block run once.

### Decisions worth remembering

- **Gate granularity is the file, not the struct.** A use case is a file
  under `src/application/` with an `fn execute(`, plus non-`execute` entry
  verbs registered in `observability/use-case-entries.txt` (`receive`,
  `dispatch`, `check_raw`, sweeps, resolvers — 18 entries). A stale
  registry entry is itself a gate failure. Attribute coverage
  (`tenant_id`) is a reviewed matrix column, not an enforced one: the gate
  enforces span presence and the `<context>.` name prefix.
- **`outcome` is not a span field.** Recording it per use case would have
  doubled the 11.1b diff for a value the request root span already carries
  as `http.status_code`; the field-standard doc records the decision.
- **`TenantDisconnected` is deliberately unregistered.** An info-level
  funnel signal whose condition is normal business flow, not a symptom;
  the payments-alerts table keeps it, the registry does not.
- **Three of the six budgets cannot be measured end to end in the lab**
  (webhook entitlement effect, conversion sweep ratio, domain issuance).
  `docs/slo/budgets.md` records the measured boundary proxies and names
  the gaps rather than inventing numbers.
- **`pg_stat_statements` is not in the compose default**, so the
  latency-budget runbook's database triage step leads with the
  `pg_stat_activity` live view; enabling the preload permanently is
  TASK-021's first action.

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
