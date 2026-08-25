# TASK-020: 11.1 Observability completion, SLOs & alerting

**Phase:** 11
**Status:** todo
**Requirement(s):** FR-1105, FR-1106, FR-1107, NFR-1103, NFR-1111
**Depends on:** TASK-019
**Created:** 2026-08-21

**Slice:** 11.1 — Observability completion, SLOs & alerting
**Prerelease:** `v1.0.0-rc.2` · **Flag:** `platform.tail_sampling`

## Context

Close the gap between "we emit telemetry" and "we can answer a question at 3 a.m." Every use case has a
span, every external call is a child span, every alert maps to user pain and has a runbook, and every
SLO has an owner who agreed to it before GA rather than during an incident.

This task blocks TASK-021 and frontend TASK-022 for a reason: **you tune what you can see.** Profiling
before trace coverage is complete means the slow dependency is the one with no span, and it stays slow
because it stays invisible.

Both tracks land together here because a field incident crosses the boundary. A slow page is either a
slow query or a heavy bundle, and the only way to tell in under a minute is a trace id that appears in
both systems.

## What to do

- [ ] **Contract** — no `/api/v1` shape changes. Two cross-cutting conventions are fixed and documented
      once in the shared schemas rather than per context: server-rendered documents and API responses
      carry the W3C `traceparent`, and every problem-details error carries a `trace_id`.
- [ ] **Trace coverage audit** — enumerate every application use case across the thirteen contexts and
      assert a span exists, named `context.usecase`, with tenant id, actor, and outcome as attributes.
      Every outbound call (Postgres, Redis, Kratos, Keto, Stripe, ACME/DNS, Google/Meta Ads, object
      storage) is a child span with peer attributes. The audit output is a **committed matrix**, not a
      claim, and a CI test walks the registered use cases and fails on an unspanned one.
- [ ] **Tail-based sampling** — sample on errors and latency outliers rather than uniformly, behind
      `platform.tail_sampling`. Head sampling is retained as the fallback when the collector is
      unavailable: losing traces must never cost requests.
- [ ] **Log hygiene** — a CI scanner runs the log emitter over a fixture corpus containing every
      PII-shaped value the platform holds (emails, phone numbers, addresses, tokens, card metadata,
      hashed identifiers, a base64-wrapped token) and **fails the build** on any leak. Field names
      standardised across contexts; levels tuned for production volume with a documented cost estimate
      per level.
- [ ] **Dashboards** — one per bounded context (RED plus that context's domain metrics: entitlement
      grants, checkout outcomes, domain issuance states, conversion upload results) and one per user
      journey (funnel views crossing contexts). Dashboards are code, in the repo, reviewed like code.
- [ ] **SLOs committed** — the six journeys get an SLI definition with its exact query, a target
      confirmed against TASK-019's baseline, a dashboard, and a named owner. Where the baseline says a
      proposed target is unreachable, the target is renegotiated **now** and the reason recorded.
- [ ] **Burn-rate alerting** — multi-window, multi-burn-rate alerts per SLO, split into paging and
      ticketing tiers. Symptom-based only: no alert on CPU, on queue depth, or on any other cause that
      does not by itself mean a user is having a bad time.
- [ ] **Error budget policy** — written, agreed, and published before GA: burning > 50 % of a monthly
      budget freezes feature merges for that context until reliability work lands. Included in
      `CONTRIBUTING.md` so it binds the team, not just the on-call.
- [ ] **Runbook completeness** — `docs/runbooks/` gains one per alert (detection, triage, mitigation,
      escalation), extending the existing eight. A CI check cross-references alert rules and runbook
      files **in both directions**: an alert with no runbook fails, a runbook with no alert fails.

## Acceptance criteria

- [ ] Every use case and every external call is spanned; the coverage matrix is committed.
- [ ] The span audit test fails when a new use case is added without instrumentation — proven in the PR
      by adding one.
- [ ] The log PII scanner blocks in CI with zero findings on the corpus, and catches every value in it
      including the base64-wrapped token and a hashed identifier that is still an identifier.
- [ ] Thirteen context dashboards and six journey dashboards are live and defined in code.
- [ ] Six SLOs each have an SLI query, a target confirmed against the baseline, a dashboard, a burn-rate
      alert, and a named owner; any renegotiated target records why.
- [ ] Alert rules are unit-tested against recorded metric series: a synthetic 10-minute outage pages, a
      single failed request does not.
- [ ] The error budget policy is published and linked from the contributing guide.
- [ ] The runbook/alert cross-reference check is green and fails on a deliberately orphaned rule.
- [ ] An e2e asserts the trace id in the SSR document equals the id in the API span and the id in a
      forced client-side error report.

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

Any performance change (TASK-021), the SSR cache and its hit-ratio metric (TASK-022), alerts for
machinery that does not exist yet — shedding (TASK-023), quotas (TASK-026), archival (TASK-028) — each
of which ships its own alerts and runbooks in its own slice.

## Files likely touched

- `crates/platform/telemetry/**` (span helpers, tail sampling, field standardisation)
- every context's `application/**` where a use case is missing a span
- `scripts/check-trace-coverage.*`, `scripts/check-log-pii.*`, `scripts/check-runbook-xref.*`
- `observability/dashboards/**`, `observability/alerts/**` (dashboards and rules as code)
- `docs/runbooks/**` (one per alert), `docs/slo/**`, `CONTRIBUTING.md`
- shared problem-details schema (`trace_id`)

## Notes / gotchas

- Rollback: `platform.tail_sampling` off returns to head sampling; dashboards and runbooks are additive.
  Alert rules ship **staged** — ticket tier first, paging tier after a week of observed
  signal-to-noise — so a noisy rule degrades to a ticket rather than to a disabled alertmanager.
- The SLO targets in the implementation plan are proposals. If four weeks of data says a target is
  unreachable, renegotiate it here and record the reason; do not carry an aspirational target into GA
  and discover it during the first incident.
- A runbook that has only been read by its author has not been tested. Each one is walked through by
  someone who did not write it before this task closes.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
