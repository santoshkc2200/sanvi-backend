# TASK-008: 09.7 Disconnect, hardening & release

**Phase:** 09
**Status:** done
**Requirement(s):** FR-910, NFR-901, NFR-902, NFR-908
**Depends on:** TASK-006, TASK-007
**Created:** 2026-08-20

**Blocked by (cross-repo):** sanvi-frontend TASK-008 — `v0.10.0` is not tagged until both tracks' 09.7 are done
**Slice:** 09.7 — Disconnect, hardening & release
**Release:** `v0.10.0` · **Flags:** default on at the end of this task

## Context

Make the phase safe to leave running unattended: a disconnect path that cannot strand money, the
observability to notice trouble before tenants report it, the runbooks for when it happens anyway,
and the full acceptance sweep across both parent plans.

Completing this task unlocks phase 10, whose conversion pipeline consumes the `PaymentSucceeded`
events from TASK-005.

## What to do

- [ ] **Contract** — `DELETE /api/v1/tenant/payments/connections/{id}` (`payments.manage`, guarded).
      Returns `409` with a structured blocker list (`in_flight_payments`, `open_disputes`,
      `pending_payouts`) when refused, so the UI can name them rather than saying "not allowed".
- [ ] **Disconnect guards** — refuse while any payment is in flight, any dispute is open, or a payout
      is pending. These are exactly the states where losing the connection loses the tenant's ability
      to resolve something with their own money.
- [ ] **Disconnect semantics** — mark the connection `Disconnected` locally, stop new checkouts, keep
      every historical record. We do **not** delete or close the Stripe account: it is the tenant's
      account, and closing it is theirs to do in their Dashboard.
- [ ] **Retention override** — payment records fall under the phase-05 retention framework with a
      statutory override (financial records outlive an erasure request; an erasure must not delete a
      transaction record). Register the override explicitly in the privacy data map so the privacy
      centre reports it accurately.
- [ ] **Reconnect path** — a tenant who disconnects and reconnects gets a new connection row;
      historical payments stay attached to the old one. The TASK-001 partial unique index permits
      exactly this.
- [ ] **Observability** — the full metric set: `connect_onboarding_started/completed_total`,
      `connection_status_gauge{status}`, `checkout_created_total{result}`, `payment_succeeded_total`,
      `payment_failure_total{code}`, `connect_webhook_lag_seconds`, `dispute_open_gauge`, plus refund
      and payout counters. Traces span the checkout path end to end, including the outbound Stripe
      call.
- [ ] **Alerts** — connections stuck in `Onboarding` > 7 days; capability regression to `Restricted`;
      webhook backlog or DLQ depth above threshold; payout failure; checkout failure-rate spike; and
      **any platform-balance movement from a tenant transaction** — the invariant alarm. It should be
      impossible, so it pages.
- [ ] **Runbooks** (`docs/runbooks/`) — restricted account, failed payout, dispute handling, Stripe
      outage / degraded mode, webhook backlog and DLQ replay, orphaned account adoption,
      accidental-fee-enablement rollback.
- [ ] **API version upgrade drill** — replay stored Connect webhook payloads against the handlers to
      prove a pinned-version bump is verifiable rather than hopeful. Document the drill; it is the
      procedure for every future Stripe version bump.
- [ ] **Release** — flags default on, changelog generated from Conventional Commits, tag `v0.10.0`,
      update [the shared roadmap](../../shared/roadmap.md).

## Acceptance criteria

Phase-09 backend acceptance, verified as a set before tagging:

- [ ] Disconnect is refused with an in-flight payment, with an open dispute, and with a pending
      payout — each returning the correct blocker code.
- [ ] Disconnect is allowed when clean, and checkout immediately refuses afterwards.
- [ ] Reconnect creates a new connection and the old payments stay queryable.
- [ ] An erasure request against a customer with payments handles personal data per phase 05, retains
      transaction records under the override, and the privacy centre reports it correctly.
- [ ] A tenant connects Stripe through embedded onboarding and reaches `Active` with
      `configuration.merchant.capabilities.card_payments.status == 'active'`.
- [ ] A sandbox purchase settles into the **tenant's** balance with no platform balance movement.
- [ ] Refunds work, are idempotent, and are correct in JPY and USD.
- [ ] `just connect-invariants` is green across the whole repo — no deprecated Connect pattern.
- [ ] Adding a second provider requires only a new adapter, proven by the fake provider in tests.
- [ ] Load: checkout creation at expected peak with webhook processing keeping up (lag stays under
      the alert threshold).
- [ ] Chaos: Stripe unavailable during checkout creation, during webhook processing, and mid-onboarding
      each degrades with a specific message and recovers without manual repair.
- [ ] Metrics, alerts and runbooks exist for every failure mode above.
- [ ] Threat model reviewed; PCI scope confirmed SAQ-A.

## Verification

```bash
just ci                       # fmt, clippy, layering, connect-invariants, test, openapi, i18n
just connect-invariants-self-test
cargo test -p sanvi-payments
just audit-verify
./scripts/smoke.sh
```

## Out of scope

Anything phase 10 consumes beyond emitting the events TASK-005 already emits. Do not build the
conversion pipeline here.

## Files likely touched

- `crates/contexts/payments/src/application/disconnect.rs`
- `crates/contexts/payments/src/adapters/metrics.rs`
- `crates/contexts/privacy/**` (retention override registration)
- `docs/runbooks/*.md`, `docs/security/threat-model.md`
- `api/payments.yaml`, `CHANGELOG.md`, [the shared roadmap](../../shared/roadmap.md)

## Notes / gotchas

- The phase-level rollback order is `payments.checkout` off first (stop taking new money), then
  `payments.stripe_connect` (stop new connections). **Never** roll back the webhook endpoint or the
  webhook handlers — in-flight payments must continue to be recorded, or money moves with no local
  record and reconciliation becomes manual.
- The platform-balance alert is the invariant alarm. Wire it even though it should never fire; a
  silent violation is exactly the failure this phase is designed to prevent.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-008 done --note "<PR or commit>"`
