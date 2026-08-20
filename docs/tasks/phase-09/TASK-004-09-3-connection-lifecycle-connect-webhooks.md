# TASK-004: 09.3 Connection lifecycle & Connect webhooks

**Phase:** 09
**Status:** todo
**Requirement(s):** FR-903, FR-904, NFR-905
**Depends on:** TASK-003
**Created:** 2026-08-20

**Slice:** 09.3 — Connection lifecycle & Connect webhooks
**Prerelease:** `v0.10.0-alpha.4` · **Flag:** `payments.stripe_connect`

## Context

The connection row must tell the truth about what the tenant can do. This task builds the whole
webhook transport — signature verification, dedupe, store-then-process, DLQ — **once**, so TASK-005
through TASK-007 only add handlers.

This is a hard blocker for checkout (TASK-005). Truth about money comes from webhooks; shipping
checkout first would make the redirect the source of truth, and that habit is very hard to remove.

## What to do

- [ ] **Contract** — no new public endpoints. `GET /api/v1/tenant/payments/connections/{id}` becomes
      fully populated:
      ```jsonc
      { "id": "…", "provider": "stripe_connect", "status": "onboarding",
        "can_accept_payments": false,
        "capabilities": { "card_payments": "pending", "payouts": "inactive" },
        "requirements": { "currently_due": [ { "code": "individual.verification.document",
                                               "summary_key": "payments.req.id_document" } ],
                          "eventually_due": [ … ], "deadline": "2026-09-30T00:00:00Z" },
        "blockers": [ { "summary_key": "payments.blocker.card_payments_inactive" } ],
        "last_synced_at": "…" }
      ```
      `can_accept_payments` is computed server-side from
      `configuration.merchant.capabilities.card_payments.status == "active"`. The frontend never
      recomputes readiness from capability strings — one definition, one place.
      `POST /internal/webhooks/stripe/connect` is internal and stays out of the public spec.
- [ ] **Connect webhook endpoint** — `/internal/webhooks/stripe/connect`, **separate** from the
      phase-04 platform endpoint, with its own signing secret. Signature verified on the raw body
      before any parsing; Stripe IPs allow-listed; body size capped. The TASK-001 cross-secret test
      already guards against the two endpoints validating each other's events.
- [ ] **Store-then-process** — persist to `payments.webhook_events` keyed by `stripe_event_id`, ack
      fast, process asynchronously in `apps/worker`. Reuse the phase-04 pattern; do not re-invent it.
- [ ] **Dedupe & ordering** — the event-id primary key makes redelivery a no-op; out-of-order
      protection compares the event's `created` against `last_synced_at` and drops stale ones. Stripe
      guarantees neither once nor in-order delivery, and both cases are tested.
- [ ] **Unknown-account rejection** — map the event's `account` field to a tenant via
      `external_account_id`. If unknown: record, alert, **do not create rows**. An unknown account is
      either a leaked endpoint or an orphan from TASK-003's failure path — never a signal to provision.
- [ ] **Handlers for this slice** — `account.updated`, `capability.updated` and the `v2.core.account…`
      events: refresh `capabilities` and `requirements`, drive the TASK-002 state machine, emit
      `ProviderConnectionActivated` / `ProviderConnectionRestricted`.
- [ ] **Reconciliation job** (`apps/worker`) — periodic status sync for connections not seen recently,
      plus the orphan adoption path from TASK-003. Webhooks are the fast path; the job is the safety
      net that makes a lost webhook a delay instead of a bug.
- [ ] **DLQ** — events failing N attempts move to a dead-letter state with the error, alertable and
      replayable from `apps/cli`. A poison event must never block the queue.
- [ ] **Requirement mapping** — Stripe requirement codes → i18n message keys, as data (`configs/` or a
      seeded table), with a documented fallback for unmapped codes showing the raw code plus a Stripe
      help link. New Stripe requirements must degrade, not break.
- [ ] **Notifications** — tenant-admin notification on activation, on regression to `Restricted`, and
      on an approaching requirement deadline, through the phase-05/06 notification path.

## Acceptance criteria

- [ ] Onboarding → `Active` is driven end to end by webhooks in the sandbox.
- [ ] Bad signature, replayed signature, and the phase-04 platform secret against the Connect
      endpoint are all rejected.
- [ ] The same event id delivered five times produces exactly one state change.
- [ ] `active` followed by a stale `pending` leaves the connection active.
- [ ] An event for an unknown account is recorded and alerted, and creates no rows.
- [ ] A poison event is dead-lettered, alerted, replayable from `apps/cli`, and does not block the
      events behind it.
- [ ] **Lost-webhook drill:** with webhooks disabled entirely, a sandbox account still reaches
      `Active` via the reconciliation job alone.
- [ ] `can_accept_payments` is computed only from the v2 capability path; a test fails if
      `charges_enabled` is reintroduced anywhere.
- [ ] Webhook lag, backlog and DLQ depth are exported as metrics with alerts.

## Verification

```bash
just fmt
just clippy
just layering
just connect-invariants
cargo test -p sanvi-payments
just test
just openapi
just i18n
```

## Out of scope

Checkout and payment-event handlers (TASK-005), refund/dispute handlers (TASK-006), payout handlers
(TASK-007). This task ships the transport plus the account/capability handlers only.

## Files likely touched

- `crates/contexts/payments/src/adapters/http/webhooks.rs`
- `crates/contexts/payments/src/application/{process_webhook,sync_connection}.rs`
- `crates/contexts/payments/src/domain/webhook.rs`
- `apps/worker/**`, `apps/cli/**` (DLQ replay)
- `configs/**` (requirement-code → message-key map), i18n catalogs

## Notes / gotchas

- **Rollback keeps the webhook endpoint live.** Rolling it back loses events permanently — Stripe
  retries for a bounded window only. Flag off stops new connections; events keep being stored.
- Verify the signature on the *raw* body. Any framework that parses before you verify has already
  lost.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-004 done --note "<PR or commit>"`
