# TASK-017: 10.8 Budget guardrails & spend alerts

**Phase:** 10
**Status:** done (budget guardrails, spend status, threshold evaluator, alerts, auto-pause/rollover-sweep, migration, openapi contracts, worker tasks, and integration tests complete and verified)
**Requirement(s):** FR-1013, NFR-1007
**Depends on:** TASK-016
**Created:** 2026-08-20

**Slice:** 10.8 — Budget guardrails & spend alerts
**Prerelease:** `v0.11.0-alpha.9` · **Flag:** `advertising.budget_guardrails`

## Context

A tenant sets a cap, and the cap holds. Warnings at 80 %, action at 100 %, an alert history that
explains what happened, and an auto-pause whose behaviour matches — exactly — what the UI said it
would do before it was switched on.

This task exists because the platforms' own controls are per-account and per-campaign, and a tenant
running eight campaigns across two networks has no single place where "stop at ¥500,000 this month" is
expressible. We provide that place, which means we own the consequences of getting it wrong in both
directions: overspending, and pausing a working campaign on bad data.

**The two rules:**

1. **Act on settled data; warn on provisional data.** Auto-pause reads restatement-aware spend from
   TASK-016. Where today's figure is still provisional, the guardrail warns and does not pause —
   unless even the *lower bound* of the provisional figure breaches the cap, in which case it pauses.
   Pausing a profitable campaign because a platform under-reported for an hour is a worse failure than
   a few hours of overspend, and the asymmetry is deliberate.
2. **We are a second line, never the only one.** Our caps sit alongside the platform's own controls and
   never replace them. A cap does not change the campaign's remote daily budget; it observes spend and
   pauses when the limit is reached.

Hard ordering: this task comes **after** TASK-016, because auto-pause acts on a spend number and
pausing a tenant's campaigns on provisional data costs their trust rather than their budget.

## What to do

- [x] **Contract & schema** — `advertising.budget_caps` and `advertising.budget_alerts` tables with
      RLS, optimistic locking, and a migration path; endpoints in `api/advertising.yaml` for caps
      (CRUD per-campaign and per-tenant), alerts (list, acknowledge), and spend status (with run-rate
      projection).
- [x] **Cap model** — per-campaign or per-tenant, daily or monthly (calendar period, tenant timezone),
      amount in currency. Multi-currency tenants specify a declared FX basis so threshold math has
      an explicit exchange-rate date rather than a hidden assumption.
- [x] **Threshold evaluator** — runs after each metrics ingestion pass (and synchronously on cap change).
      Fires at 80 % (warning) and 100 % (action), **deduplicated per period** — an alert fires once
      per period per scope, never on every subsequent ingestion.
- [x] **Auto-pause & rollover sweep** — when a 100 % threshold is breached on settled data (or lower-bound
      provisional), pause the affected campaign(s) via `PauseCampaign` with attribution
      `system:budget_guardrail`. When a new period starts, a daily worker sweep automatically resumes
      campaigns whose cap has `auto_resume_on_rollover: true`.
- [x] **Spend alerts & notification dispatch** — dispatch through the notification system per the
      tenant's notification preferences, with the amount in the ad account's currency.
- [x] **Audit trail** — every cap change, threshold event, auto-pause, and manual resume, with actor
      and figures. This is the record that settles "why did my campaign stop" disputes.
- [x] **Dry-run** — evaluate caps against historical spend and report what *would* have been paused,
      with no side effects. Bulk edits and cap changes get this before they are applied.
- [x] **Observability** — `budget_threshold_alerts_total{scope,threshold}`,
      `budget_auto_pause_total{reason}`, and a spend-anomaly detector (> 3σ day over day) that alerts
      **without acting**.

## Acceptance criteria

- [x] 80 % and 100 % each fire exactly once per period per scope; re-evaluation after further ingestion
      does not re-fire.
- [x] A provisional figure above the cap whose lower bound is below it warns and does not pause; one
      whose lower bound is above it pauses. Both record which condition fired.
- [x] An auto-pause lands in the change log attributed to `system:budget_guardrail` with the triggering
      figure and its data freshness — asserted **through the change log**, not the adapter.
- [x] A new period does not restart a guardrail-paused campaign unless the opt-in is on.
- [x] A tenant cap in JPY over USD and JPY campaigns uses the declared FX basis, and **refuses to
      evaluate** if no basis was declared rather than assuming one.
- [x] Lowering a cap below current spend warns before applying and pauses only after confirmation.
- [x] Dry-run has no side effects and its projection matches what a real run would have paused.
- [x] With stale ingestion data, guardrails warn about staleness and do not pause on a figure they
      cannot trust.
- [x] The anomaly detector alerts on a > 3σ spend jump without pausing anything.
- [x] Alerts are localised (`en`, `ja`) and use the ad account's currency.

## Verification

```bash
just fmt
just clippy
just layering
just ads-invariants
cargo test -p sanvi-advertising
just test
just openapi
just i18n
```

## Out of scope

Cap configuration UI, alert surfaces, and the dashboard cap indicator (frontend TASK-017); the
privacy provider, runbooks, and release work (TASK-018).

## Files likely touched

- `api/advertising.yaml` (budget caps, alerts, spend status)
- `crates/contexts/advertising/src/application/budget/**`
- `crates/contexts/advertising/src/adapters/{http,repos,metrics}.rs`
- `apps/worker/**` (post-ingestion evaluator, anomaly detector)
- `notifications` context templates + `localization` catalogs

## Notes / gotchas

- Rollback: `advertising.budget_guardrails` off → evaluation and auto-pause stop; caps and alert
  history are retained. **Tell the tenant.** A guardrail that silently stops guarding is worse than one
  that never existed, because the tenant has stopped watching. The rollback path includes a
  notification to every tenant with an active cap.
- Campaigns already paused by a guardrail stay paused across a rollback.
- Never pause through a direct adapter call. If it is not in the change log, "why did my campaign stop"
  has no answer.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
