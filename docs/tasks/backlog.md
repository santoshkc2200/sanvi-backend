# Backlog — sanvi-backend

Single source of truth for task status. Maintained by hand, in the same commit as the task file
it describes.

**Recording status.** A status change touches exactly two places, and they must never disagree:

1. the `**Status:**` line at the top of `phase-NN/TASK-0NN-….md`, and
2. that task's row in the table below, with the PR or commit in the **Notes** column.

**Adding a task.** Take the next free `TASK-0NN` — IDs are permanent and never reused — name its
requirement from [`../requirements.md`](../requirements.md), and add it both here and as a file in
the phase's `tasks/phase-NN/` directory. Two invariants hold over the set, and a reviewer checks
them by reading: every requirement is covered by at least one task, and every task names a
requirement.

**Executing a task.** Tasks are executed with the Superpowers workflow — `superpowers:executing-plans`
for inline execution, or `superpowers:subagent-driven-development` for a fresh subagent per task with
review between tasks. The checkboxes inside a task file are the working record of that execution;
this table is the summary.

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
| TASK-009 | 10 | 10.0 Contract, schema & foundations | in-progress | phase 03, 05, 08, 09 | code complete (62ed843, 202b224): api/advertising.yaml, sanvi-advertising crate, migration 20260826000000 + 7 follow-ups (8 tables, RLS, monthly partitions on conversion_events/metrics_daily), retention rules in privacy.retention_rules, 6 advertising.* permissions + 3 entitlements, per-tenant KEK token vault, `advertising = false` flag, `just ads-invariants` (+ self-test) in `just ci`. Pending: staging deploy per DoD |
| TASK-010 | 10 | 10.1 Domain model, capability matrix & fake adapter | in-progress | TASK-009 | code complete (5ad9498): domain/{model,matrix,validation,ports,events}.rs, AdPlatformPort (auth, list_accounts, campaign CRUD, pause/resume/delete, fetch_metrics, upload), FakeAdPlatform + fixtures/capability-matrices.json exported to sanvi-frontend, outbox event round-trip green, GET /ads/platforms catalog. Pending: staging deploy per DoD |
| TASK-011 | 10 | 10.2 Connections, OAuth & token lifecycle | in-progress | TASK-010 | shipped (c30721b, 7b69878, b35a5d4): signed-state OAuth start/callback rejecting forgery/expiry/replay/cross-tenant redemption, per-tenant encrypted refresh tokens, refresh scheduler + expiry, pending/reconnect lifecycle, FreshAal2Required step-up on connect+disconnect, health from each adapter's required_scopes. Still open: real Google & Meta OAuth adapters and live list_accounts — blocked on Google developer-token approval and Meta app review; fake adapter covers the interim |
| TASK-012 | 10 | 10.3 Campaign tree, Google Ads adapter & drift | in-progress | TASK-011 | shipped (03b0108, 2e8ff46, c7b2941): campaign intent snapshots, optimistic revisions (409 on stale), mutation change log, draft/validate/publish/patch/pause/resume + ad-groups/ads, drift detection with KeepTheirs/ReapplyOurs, nightly cross-tenant ReconcileDueCampaigns sweep in apps/worker (drift-only, never writes back), RateLimitedAdapter per-connection + platform-wide quota queue in api and worker, all against the fake adapter with integration coverage. Still open: real Google Ads adapter (blocked on developer-token access) and review-status polling (needs the real adapter to have meaning) |
| TASK-013 | 10 | 10.4 Meta adapter & creatives | in-progress | TASK-012 | creative CRUD (contract, `CreativeRepository`, `application::creatives`, HTTP), upload-time spec/text validation wired in, broadened Meta capability matrix (6 objectives, 2 budget types, 3 placements), per-platform rate-limit budgets (`rate_limited_registry` now per-`PlatformKey`), Meta drift test added with zero engine diff. Still open: real Meta Marketing API adapter (blocked on Meta app review, same as TASK-011/012's Google adapter), `add_ad`/`update_ad` creative_id validation, campaign-budget-optimization `BudgetType` variant |
| TASK-014 | 10 | 10.5 Conversion capture & the directive gate | in-progress | TASK-011 | directive gate, hashing, idempotent capture, tracking settings/test-event/conversions endpoints and OpenAPI contract are code-complete and tested (unit + Postgres-backed integration: all 4 directive scenarios, vendor-role branching, JPY payment-record override, dedupe, click-id storage, PII-free persistence). Still open: server-issued event_id from the phase-09 order record, mounting /public/track on the tenant's own phase-08 domain (currently a shared-domain endpoint gated by site key + Origin), server-side click-id extraction from cookies/URL, and HTTP-level tests of the abuse-control paths |
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
