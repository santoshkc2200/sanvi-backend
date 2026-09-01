# TASK-018: 10.9 Privacy integration, hardening & release

**Phase:** 10
**Status:** in-progress
**Requirement(s):** FR-1014, NFR-1001, NFR-1004, NFR-1011
**Depends on:** TASK-013, TASK-015, TASK-017
**Created:** 2026-08-20

**Blocked by (cross-repo):** sanvi-frontend TASK-018 must also be `done` before `v0.11.0` is tagged
**Slice:** 10.9 — Privacy integration, hardening & release
**Release:** `v0.11.0` · **Flags:** default on at the end of this task

## Context

Make the phase safe to leave running unattended: the privacy context can export and erase what
advertising holds, the sub-processor registry tells the legal truth, observability catches trouble
before tenants report it, the runbooks exist for when it happens anyway, and both parent plans'
acceptance criteria are verified as a set.

## What to do

- [ ] **Contract** — no new tenant endpoints. Add `GET /api/v1/platform/ads/health` (platform-admin),
      plus the phase-05 `PersonalDataProvider` implementation for this context — an in-process port,
      not an HTTP surface, consumed by the existing privacy export and erasure flows.
- [x] **`PersonalDataProvider`** — export and erase advertising-held personal data: conversion
      identifiers (hashes), click ids, consent snapshots, and audience membership. Two decisions made
      explicit rather than implicit:
      - a hashed identifier **is** personal data and is exported and erased accordingly;
      - a consent snapshot is **retained** under the phase-05 statutory/legitimate-interest override,
        because it is the proof that a directive was honoured. Erasing the proof of compliance to
        satisfy an erasure request is the wrong trade, and the override is registered in the data map
        so the privacy centre reports it accurately.
- [x] **Erasure propagation** — an erasure request triggers TASK-015's platform deletion/suppression
      requests and removal from every audience list on the next refresh, with outcomes recorded per
      platform.
- [x] **Sub-processor registry** — register Google and Meta in the phase-05 registry with their
      **role** (service provider/contractor vs third party — the field that decides whether an upload
      is a "sale" or "share" under US law), the contract terms relied on, and transfer disclosures for
      APPI cross-border provision and EU/UK transfers to US platforms. The registry is what TASK-014's
      gate reads; a wrong role here makes a correct gate produce a wrong answer.
- [ ] **Observability completion** — the full metric set: `ads_sync_duration{platform}`,
      `ads_sync_failures_total{platform,code}`, `conversion_upload_total{platform,result}`,
      `conversion_suppressed_total{reason,source}`, `ads_token_refresh_failures`,
      `metrics_ingestion_lag_hours`, `budget_threshold_alerts_total`. Traces span the tracking path end
      to end, including the outbound platform call.
- [ ] **Alerts** — ingestion lag > 6 h; upload failure ratio > 5 %; token refresh failures; spend
      anomaly (> 3σ day over day); DLQ depth; **any upload attempted without a stored consent
      snapshot** — that last one should be impossible, so it pages.
- [ ] **Runbooks** (`docs/runbooks/`): token revoked or app uninstalled; platform API outage and
      degraded mode; conversion upload failure surge; spend anomaly; ingestion backlog and restatement
      recovery; audience list retraction; accidental auto-pause; partition maintenance failure.
- [ ] **Platform version-bump drill** — replay recorded platform fixtures against both adapters to
      prove a pinned API version bump is verifiable rather than hopeful. Document the drill; it is the
      procedure for every future bump, and both platforms deprecate on a schedule.
- [ ] **Retention verification** — confirm TASK-009's retention rules actually fire: aged
      `conversion_events` partitions are dropped, `metrics_daily` follows its own longer window, and
      both are reported in the privacy centre.
- [ ] **Release** — flags default on, changelog generated from Conventional Commits, tag `v0.11.0`,
      update `docs/shared/roadmap.md`.

## Acceptance criteria — phase 10 acceptance

Both parent plans' criteria, verified as a set before tagging:

- [ ] A tenant connects Google Ads and Meta, creates a campaign in Sanvi, and it appears live on the
      platform with matching budget and schedule.
- [ ] A storefront purchase produces exactly one conversion on each connected platform, with the value
      taken from the server-side payment record.
- [ ] With `ads_measurement` withdrawn, or with a US opt-out of sale/share including one expressed only
      via GPC, no conversion is uploaded and the suppression is visible in diagnostics with the purpose
      and source that caused it.
- [ ] The dashboard shows spend, clicks, conversions, revenue, and ROAS for a date range, matching the
      platform's own reporting within the documented restatement window, with both attribution sources
      labelled.
- [ ] Recent-day restatement is visibly flagged rather than silently wrong.
- [ ] A campaign paused natively is reported as drifted with a diff and an explicit resolution choice,
      never overwritten.
- [ ] Budget threshold alerts fire and auto-pause behaves exactly as the UI described.
- [ ] Adding a third network requires only an adapter plus a capability matrix — proven with the fake.
- [ ] An opted-out subject is absent from new audience lists and removed from existing ones on refresh.
- [ ] An export request for a customer with conversions returns hashed identifiers, click ids, and
      audience membership; erasure removes them, retains the consent snapshot under the override,
      propagates to both platforms, and the privacy centre reports all of it accurately.
- [ ] A sub-processor **role change flips the gate outcome** for the same subject and event — the test
      that proves the registry is wired to TASK-014 rather than decorative.
- [ ] An aged partition is dropped on schedule and the drop is reported.
- [ ] Chaos: each platform unavailable during connect, publish, ingestion, and upload — each degrades
      with a specific message and recovers without manual repair.
- [ ] The version-bump drill executes green for both adapters against recorded fixtures.
- [ ] Load: 500 campaigns × 90 days ingestion within the scheduling window, concurrent with upload
      workers at peak conversion volume.
- [ ] Full CI gate sweep re-run: no raw PII field in adapters, no loggable secret, no upload path
      without a consent snapshot, no blended ROAS field.
- [ ] No token, client secret, or raw PII exists in any log, trace, bundle, or export (CI-asserted).
- [ ] Metrics, alerts, and runbooks exist for every failure mode above.
- [ ] Threat model reviewed; sub-processor registry and transfer disclosures updated and signed off.

## Verification

```bash
just fmt
just clippy
just layering
just ads-invariants
just ads-invariants-self-test
just ci
cargo test -p sanvi-advertising
just openapi
just i18n
```

## Out of scope

Phase 11's cross-cutting hardening (performance budgets, DR drills, external audit) — this task
hardens phase 10's own surface only.

## Files likely touched

- `crates/contexts/advertising/src/adapters/privacy.rs` (`PersonalDataProvider`)
- phase-05 sub-processor registry + data map entries
- `crates/contexts/advertising/src/adapters/metrics.rs`, alert rules in `configs/`
- `docs/runbooks/*.md` (eight new runbooks)
- `api/advertising.yaml` (platform health endpoint), `docs/shared/roadmap.md`, `CHANGELOG.md`

## Notes / gotchas

- Phase-level rollback order: `advertising.dashboard` (numbers go dark, ingestion continues), then
  `advertising.conversion_tracking` uploads, then the per-platform write flags, then
  `advertising.budget_guardrails` **with tenant notification**, then `advertising.enabled`. The
  guardrail goes last, not first — it is the thing you want still running while other parts are
  disabled.
- **Never** roll back the ingestion scheduler or the token refresh scheduler. Ingestion gaps cannot be
  fully backfilled once platform reporting windows expire, and stranded tokens force every tenant
  through re-consent.
- Campaigns live on the platforms keep spending throughout any rollback; every disabled screen must say
  so rather than implying that advertising has stopped.
- `v0.11.0` cannot be tagged until `sanvi-frontend` TASK-018 is also `done`.

---
*On completion: satisfy every acceptance criterion, run the verification commands, then
record status in the **same commit** in both places — the `**Status:**` line at the top of this
file and this task's row in [`../backlog.md`](../backlog.md), with the PR or commit as the note.
The two must never disagree.*
