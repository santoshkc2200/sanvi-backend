# TASK-017: 10.8 Budget guardrails & spend alerts

**Phase:** 10
**Status:** todo
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
   never replace them. If our worker is down, the platform's limits still apply — and a tenant relying
   solely on us has a single point of failure they did not choose.

Hard ordering: this task comes **after** TASK-016, because auto-pause acts on a spend number and
pausing a tenant's campaigns on provisional data costs their trust rather than their budget.

## What to do

- [ ] **Contract** — `GET/PUT /ads/budget-caps` (`advertising.read` / `advertising.budget.manage`),
      `GET/PUT /ads/campaigns/{id}/budget-cap`, `GET /ads/budget-alerts`,
      `POST /ads/budget-alerts/{id}/acknowledge`, `GET /ads/spend-status`. `spend-status` returns per
      scope: cap, spend to date, projected spend at the current run rate, percentage, the freshness of
      the underlying data, and the exact action configured at each threshold.
- [ ] **Cap model** — per-campaign and per-tenant, daily and monthly, with an effective currency and,
      for cross-currency tenants, a **declared FX basis and rate date**. Caps are versioned; changing
      one is an attributed change-log entry like any other money mutation.
- [ ] **Threshold evaluator** — runs after each ingestion cycle rather than on a fixed clock, so it
      always reasons over the freshest data. Emits `SpendThresholdReached` at 80 % and 100 %, each once
      per period per scope. An alert that fires every ten minutes is an alert nobody reads.
- [ ] **Auto-pause** — configurable per scope and **off by default**. On breach, pause via TASK-012's
      ordinary mutation path so the action lands in the change log attributed to
      `system:budget_guardrail` with the triggering figure and its freshness. Never a direct adapter
      call that bypasses the log.
- [ ] **Provisional-data guard** — implements rule 1: pause only when the settled figure breaches, or
      when the provisional figure's lower bound does. Record which condition fired.
- [ ] **Resume semantics** — auto-pause does not auto-resume. A new period does not silently restart
      spending; the tenant resumes explicitly. The one exception is an explicit "resume at period
      rollover" opt-in, off by default and clearly labelled.
- [ ] **Alert delivery** — through the `notifications` context, respecting locale (phase 06) and the
      tenant's notification preferences, with the amount in the ad account's currency.
- [ ] **Audit trail** — every cap change, threshold event, auto-pause, and manual resume, with actor
      and figures. This is the record that settles "why did my campaign stop" disputes.
- [ ] **Dry-run** — evaluate caps against historical spend and report what *would* have been paused,
      with no side effects. Bulk edits and cap changes get this before they are applied.
- [ ] **Observability** — `budget_threshold_alerts_total{scope,threshold}`,
      `budget_auto_pause_total{reason}`, and a spend-anomaly detector (> 3σ day over day) that alerts
      **without acting**.

## Acceptance criteria

- [ ] 80 % and 100 % each fire exactly once per period per scope; re-evaluation after further ingestion
      does not re-fire.
- [ ] A provisional figure above the cap whose lower bound is below it warns and does not pause; one
      whose lower bound is above it pauses. Both record which condition fired.
- [ ] An auto-pause lands in the change log attributed to `system:budget_guardrail` with the triggering
      figure and its data freshness — asserted **through the change log**, not the adapter.
- [ ] A new period does not restart a guardrail-paused campaign unless the opt-in is on.
- [ ] A tenant cap in JPY over USD and JPY campaigns uses the declared FX basis, and **refuses to
      evaluate** if no basis was declared rather than assuming one.
- [ ] Lowering a cap below current spend warns before applying and pauses only after confirmation.
- [ ] Dry-run has no side effects and its projection matches what a real run would have paused.
- [ ] With stale ingestion data, guardrails warn about staleness and do not pause on a figure they
      cannot trust.
- [ ] The anomaly detector alerts on a > 3σ spend jump without pausing anything.
- [ ] Alerts are localised (`en`, `ja`) and use the ad account's currency.

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
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-017 done --note "<PR or commit>"`
