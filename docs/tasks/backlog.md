# Backlog — sanvi-backend

Single source of truth for task status. Generated and updated by the sdlc-planner
script — don't hand-edit rows or hand-number IDs; the script keeps this table and
the task files in sync, and `check` flags it when they drift.

```bash
SDLC="python3 ~/.claude/skills/sdlc-planner/scripts/sdlc.py"

$SDLC check                                     # traceability + status drift (run from this repo root)
$SDLC status TASK-002 in-progress               # updates the task file AND the row below
$SDLC status TASK-002 done --note "PR #123"
$SDLC new-task 10 "<title>" --requirement FR-1003 --depends TASK-010
```

Each task traces to a numbered requirement in [`../requirements.md`](../requirements.md); the *why*
behind each one lives in that phase's implementation plan
([09](../phase-09-tenant-payments/implementation-plan.md) ·
[10](../phase-10-advertising/implementation-plan.md) ·
[11](../phase-11-hardening-ga/implementation-plan.md)) and the slice order in its
`phase-NN/README.md` ([09](phase-09/README.md) · [10](phase-10/README.md) ·
[11](phase-11/README.md)). Every task must clear
[`definition-of-done.md`](definition-of-done.md).

Statuses: `todo` · `in-progress` · `blocked` · `done`

| ID | Phase | Title | Status | Depends on | Notes |
|---|---|---|---|---|---|
| TASK-001 | 09 | 09.0 Contract, schema & foundations | done | phase 03, 04, 08 | shipped: payments crate, migration 20260823000000, catalog route, connect-invariants gate |
| TASK-002 | 09 | 09.1 Provider-agnostic domain, ports & catalog | done | TASK-001 | shipped: domain+ports+fake provider, registry-derived catalog, ProviderConnectionStarted outbox e2e (09.1) |
| TASK-003 | 09 | 09.2 Stripe Connect onboarding (Accounts v2) | done | TASK-002 | d799455 |
| TASK-004 | 09 | 09.3 Connection lifecycle & Connect webhooks | done | TASK-003 | commit 8234c33 |
| TASK-005 | 09 | 09.4 Checkout on the connected account | done | TASK-004 | 09.4 shipped: checkout sessions on the connected account (Stripe-Account direct charges), idempotent create with persist-before-provider, verified-domain allow-list, per-tenant/IP/customer throttle, 5 payment webhook handlers writing payments.payments + outbox PaymentSucceeded/PaymentFailed, server-derived conversion_event_id, contract regenerated |
| TASK-006 | 09 | 09.5 Payments, refunds & disputes | done | TASK-005 | review findings fixed in ef30312 (refund recovery + idempotent replay, dispute routing via payment_intent, deep links, streamed export); 8 acceptance tests added |
| TASK-007 | 09 | 09.6 Payouts, tax liability & application fee | done | TASK-004, TASK-005 | working tree: 09.6 payouts (read port + webhook projection + failed-payout alert), tax liability (account-liability routing + compliance preflight + scheduled/webhook re-check), per-tenant application fee (platform-admin config, round-half-up, disclosure, blocked-combination guards + CI gate) |
| TASK-008 | 09 | 09.7 Disconnect, hardening & release | done | TASK-006, TASK-007 | commit 33fa087 — 09.7 guarded disconnect, retention override, metrics/alerts/runbooks, SAQ-A review, flags default on, v0.10.0 changelog; tag pending cross-repo frontend 09.7 |
| TASK-009 | 10 | 10.0 Contract, schema & foundations | in-progress | phase 03, 05, 08, 09 | local verification complete; staging deployment and repository-layer evidence pending |
| TASK-010 | 10 | 10.1 Domain model, capability matrix & fake adapter | in-progress | TASK-009 |  |
| TASK-011 | 10 | 10.2 Connections, OAuth & token lifecycle | in-progress | TASK-010 | OAuth signed-state boundary started |
| TASK-012 | 10 | 10.3 Campaign tree, Google Ads adapter & drift | todo | TASK-011 | |
| TASK-013 | 10 | 10.4 Meta adapter & creatives | todo | TASK-012 | |
| TASK-014 | 10 | 10.5 Conversion capture & the directive gate | todo | TASK-011 | |
| TASK-015 | 10 | 10.6 Conversion upload, dedupe & diagnostics | todo | TASK-014 | |
| TASK-016 | 10 | 10.7 Metrics ingestion, rollups & ROAS query layer | todo | TASK-012, TASK-015 | |
| TASK-017 | 10 | 10.8 Budget guardrails & spend alerts | todo | TASK-016 | |
| TASK-018 | 10 | 10.9 Privacy integration, hardening & release | todo | TASK-013, TASK-015, TASK-017 | |
| TASK-019 | 11 | 11.0 Freeze, production-shaped baseline & measurement harness | todo | phases 00-10 | |
| TASK-020 | 11 | 11.1 Observability completion, SLOs & alerting | todo | TASK-019 | |
| TASK-021 | 11 | 11.2 Backend performance, load testing & capacity model | todo | TASK-020 | |
| TASK-022 | 11 | 11.3 SSR response cache, cache headers & compression | todo | TASK-020 | |
| TASK-023 | 11 | 11.4 Resilience, degradation & zero-downtime deploys | todo | TASK-021 | |
| TASK-024 | 11 | 11.5 Threat model, blocking scanners, isolation sweep & pen-test remediation | todo | TASK-019 | |
| TASK-025 | 11 | 11.6 Disaster recovery, secret rotation & on-call readiness | todo | TASK-023, TASK-024 | |
| TASK-026 | 11 | 11.7 API v1 freeze, deprecation policy, rate limits & quotas | todo | TASK-021, TASK-023 | |
| TASK-027 | 11 | 11.8 Error addressability, collection ordering & template accessibility | todo | TASK-022 | |
| TASK-028 | 11 | 11.9 Data lifecycle at scale, archival & tenant offboarding | todo | TASK-021 | |
| TASK-029 | 11 | 11.10 Compliance evidence packs & jurisdiction drills | todo | TASK-028 | |
| TASK-030 | 11 | 11.11 GA release gate, flag cleanup, staged rollout & v1.0.0 | todo | TASK-024, TASK-025, TASK-026, TASK-027, TASK-029 | |
