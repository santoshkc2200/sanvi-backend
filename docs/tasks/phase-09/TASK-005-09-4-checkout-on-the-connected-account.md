# TASK-005: 09.4 Checkout on the connected account

**Phase:** 09
**Status:** todo
**Requirement(s):** FR-905, NFR-901, NFR-904
**Depends on:** TASK-004
**Created:** 2026-08-20

**Sub-phase:** [09.4 — Checkout on the connected account (direct charges)](../../../../docs/phase-09-tenant-payments/09.4-checkout-direct-charges.md)
**Prerelease:** `v0.10.0-alpha.5` · **Flag:** `payments.checkout`

## Context

A customer buys something on the tenant's own domain and the money lands in the **tenant's** Stripe
balance. Nothing transits Sanvi. This is the slice the whole phase exists for, and the one where the
architectural invariant is either proven or quietly broken.

## The two rules

1. **Direct charges only.** The session is created on the connected account (`Stripe-Account:
   acct_…`). The platform balance must not move — asserted in the sandbox, not assumed.
2. **The webhook is the fact; the redirect is a hint.** Order state is never derived from the return
   URL. A customer who never returns still gets a paid order; a customer who forges a return URL gets
   nothing.

## What to do

- [ ] **Contract** — add to `api/payments.yaml`, merged first:
      | Method | Path | Permission |
      |---|---|---|
      | POST | `/api/v1/tenant/checkout` | `payments.checkout`, or public storefront-scoped and rate-limited |
      | GET | `/api/v1/tenant/checkout/{id}` | same scope — polled by the confirming state |

      Request carries the merchant order reference, line items or an amount, currency, customer
      reference and locale; response carries the checkout id, the Stripe-hosted URL and `expires_at`.
      `Idempotency-Key` is required — a double-clicked buy button must not create two sessions. `GET`
      returns `{ status: "pending" | "paid" | "failed" | "expired" | "canceled", … }` and is the only
      source the confirmation page trusts.
- [ ] **Create-checkout use case**
      - Load the connection; refuse unless `can_accept_payments` (the TASK-004 computed field) with
        `payments/connection-not-active` and a reason code the UI can render.
      - Validate currency and amount against the connected account's country and supported currencies
        *before* calling Stripe — a rejected session at Stripe is a worse error message than ours.
      - Validate success/cancel URLs belong to a **verified tenant domain** from phase 08. An
        open redirect here is a phishing primitive: allow-list, do not sanitize.
      - Persist the `payments.checkouts` row **before** creating the session, so an interrupted
        request is recoverable and the idempotency key has an owner.
- [ ] **Stripe session creation** — on the connected account, `mode: 'payment'`, **no
      `payment_method_types`** (dynamic payment methods; hardcoding `card` locks out the local methods
      Japanese buyers expect, and the TASK-001 grep gate enforces this), `locale` from phase 06,
      success/cancel URLs on the tenant's domain, an `integration_identifier` tag, and metadata
      carrying our checkout id and tenant id.
- [ ] **Application fee** — `payment_intent_data.application_fee_amount` is populated **only** when
      the per-tenant fee config is enabled. It is off in this release; the code path exists and is
      tested (TASK-007). Never combine it with separate charges and transfers — that combination is
      blocked and asserted against.
- [ ] **Payment webhooks** (handlers on TASK-004's transport): `checkout.session.completed`,
      `payment_intent.succeeded`, `payment_intent.payment_failed`, `payment_intent.canceled`,
      `checkout.session.expired`. Each writes a `payments.payments` row, transitions the checkout, and
      emits `PaymentSucceeded` / `PaymentFailed` to the outbox for phase 10's conversion pipeline.
- [ ] **Conversion event id** — issued server-side, once per order, carried on the confirmation
      payload so phase 10 cannot double-count a refreshed confirmation page.
- [ ] **Rate limiting & abuse** — checkout creation is rate-limited per tenant, per IP and per
      customer reference. Card-testing traffic against a connected account is the tenant's Stripe
      problem; session-creation flooding is ours.
- [ ] **Isolation guard** — an explicit assertion in the adapter that the `Stripe-Account` header
      matches the connection belonging to the *request's* tenant. RLS protects the rows; this protects
      the outbound call.

## Acceptance criteria

- [ ] **The phase's core test:** after a sandbox purchase the charge exists on the connected account,
      the platform balance is unchanged, and no `application_fee_amount` is present while fees are
      disabled.
- [ ] Checkout is refused unless `card_payments.status == 'active'`, asserted against the v2 path.
- [ ] Tenant A cannot create a checkout on tenant B's account — both the RLS path and the outbound
      header guard are tested.
- [ ] A double-click creates one session; a replayed idempotency key returns the same session.
- [ ] A success URL on an unverified domain is rejected.
- [ ] A customer who never returns still ends with a paid order; a forged return URL with no webhook
      yields no order.
- [ ] 3DS/SCA, decline, expired-session and cancel paths all behave correctly in Stripe test mode.
- [ ] A JPY purchase is correct at every layer, end to end.
- [ ] No `payment_method_types` anywhere; `just connect-invariants` is green.

## Verification

```bash
just fmt
just clippy
just layering
just connect-invariants
cargo test -p sanvi-payments
just test
just openapi
```

## Out of scope

Payment read models, refunds and disputes (TASK-006); payouts, tax liability and fee configuration
(TASK-007). This task creates checkouts and records their outcome.

## Files likely touched

- `crates/contexts/payments/src/application/create_checkout.rs`
- `crates/contexts/payments/src/adapters/stripe_connect/checkout.rs`
- `crates/contexts/payments/src/adapters/http/{handlers,routes}.rs`
- `crates/contexts/payments/src/adapters/http/webhooks.rs` (payment handlers)
- `api/payments.yaml`

## Notes / gotchas

- Rollback: `payments.checkout` off → the endpoint returns `503 payments/provider-unavailable`.
  **Keep the webhook handlers deployed even when checkout is disabled**, or money will be taken at
  Stripe with no local record.
- The domain allow-list comes from phase 08's verified domains. Do not accept a URL because it merely
  *looks* like the tenant's domain.

---
*On completion: satisfy every acceptance criterion, run the verification commands,
then record status with the sdlc-planner script (never by hand-editing this line
and the backlog separately):*
`sdlc.py status TASK-005 done --note "<PR or commit>"`
