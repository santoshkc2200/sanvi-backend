# Phase 09 — Tenant Payment Providers, Stripe Connect (backend)

**Target version:** 0.10.0
**Depends on:** phases 01, 02, 03 (entitlement `payments.stripe_connect`), 04 (Stripe plumbing), 08 (checkout on the tenant's domain)
**Unlocks:** tenants can take money from their own customers

## Goal

A `payments` bounded context that lets a tenant connect a Stripe account so **their** customers pay
**them**. Sanvi never becomes the merchant of record and never holds tenant funds. The abstraction is
provider-agnostic so a second provider can be added without touching checkout code, but this release
ships Stripe / Stripe Connect only.

## Key decision: charge pattern

Sanvi is a **SaaS platform whose tenants run their own businesses and own their own customers**. Per
Stripe's platform guidance that maps to:

| Dimension | Value | Consequence |
|---|---|---|
| Account API | **Accounts v2** (`POST /v2/core/accounts`) | Never `type: 'standard' \| 'express' \| 'custom'` — those are deprecated v1 patterns |
| Configuration | `configuration.merchant` requesting `card_payments` (+ other LPMs) | Required for a connected account to be merchant of record |
| Dashboard | `dashboard: "full"` | Tenants are independent businesses; they get the real Stripe Dashboard |
| Fees collector | `defaults.responsibilities.fees_collector: "stripe"` | The connected account pays Stripe's fees directly |
| Losses collector | `defaults.responsibilities.losses_collector: "stripe"` | Stripe owns negative-balance liability, not us |
| Charge pattern | **Direct charges** on the connected account | The tenant is merchant of record — "payment should flow to the merchant rather than the SaaS", exactly as the brief requires |
| Platform revenue on tenant transactions | `application_fee_amount` on the PaymentIntent **if and only if** the commercial model calls for it; 0.10.0 ships with it disabled (we monetise via subscription) | `application_fee_amount` is the correct mechanism for direct charges; the field exists but is configured off |
| Go-live check | `configuration.merchant.capabilities.card_payments.status === 'active'` | Never `charges_enabled` — a deprecated v1 field |

Blocked combinations we must never introduce: `losses_collector: "stripe"` with destination or
separate charges; `application_fee_amount` with separate charges and transfers.

## Scope

**In**
- Provider-agnostic domain (`PaymentProviderConnection`, `Checkout`, `PaymentIntentRef`, `RefundRef`).
- Stripe Connect onboarding with embedded components and account-status tracking.
- Tenant checkout: create payments on the connected account, handle 3DS/SCA, refunds, disputes view.
- Connected-account webhooks, separate from the platform webhook endpoint.
- Payout/balance visibility (read-only, via embedded components — we do not reimplement Stripe UI).
- Tax liability routing for tenant transactions.

**Out**
- Non-Stripe providers (PayPal, local Japanese providers) — the port exists, adapters come later.
- Subscriptions *by tenants for their own customers* (evaluated in a later phase; if built, it uses
  the connected account's v2 customer configuration and `customer_account`, not v1 Customers).
- Holding funds, escrow, split payments across multiple merchants (would require a different pattern).

## Domain model

```rust
pub struct PaymentProviderConnection {
    id, tenant_id, provider: ProviderKind /*StripeConnect*/,
    external_account_id: String,            // acct_…
    status: ConnectionStatus,               // Pending|Onboarding|Restricted|Active|Rejected|Disconnected
    capabilities: CapabilitySet,            // card_payments: active|pending|inactive, …
    requirements: RequirementSummary,       // currently due, eventually due, deadline
    country: CountryCode, default_currency: Currency,
    connected_at, last_synced_at }

pub struct Checkout { id, tenant_id, reference: MerchantOrderRef, amount: Money,
                      customer: CustomerRef, status: CheckoutStatus,
                      external_session_id: String, expires_at }
pub struct PaymentRecord { id, tenant_id, checkout_id, external_payment_id, status: PaymentStatus,
                           amount: Money, fee_amount: Option<Money>, captured_at, failure: Option<Failure> }
```

Ports: `PaymentProviderPort` (onboard, status, create checkout, refund, sync), `PayoutReadPort`,
`WebhookVerifierPort`. Stripe specifics live only in the adapter.

Events: `ProviderConnectionStarted`, `ProviderConnectionActivated`, `ProviderConnectionRestricted`,
`ProviderDisconnected`, `CheckoutCreated`, `PaymentSucceeded`, `PaymentFailed`, `RefundIssued`,
`DisputeOpened`, `PayoutPaid`.

## Deliverables

### Schema

```sql
CREATE SCHEMA payments;
CREATE TABLE payments.provider_connections (
  id uuid PRIMARY KEY, tenant_id uuid NOT NULL, provider text NOT NULL,
  external_account_id text NOT NULL, status text NOT NULL,
  capabilities jsonb NOT NULL DEFAULT '{}', requirements jsonb NOT NULL DEFAULT '{}',
  country text, default_currency text, connected_at timestamptz, last_synced_at timestamptz,
  disconnected_at timestamptz,
  UNIQUE (provider, external_account_id));
CREATE UNIQUE INDEX one_active_provider_per_tenant ON payments.provider_connections (tenant_id, provider)
  WHERE disconnected_at IS NULL;

CREATE TABLE payments.checkouts (...);
CREATE TABLE payments.payments (...);
CREATE TABLE payments.refunds (...);
CREATE TABLE payments.webhook_events (   -- connected-account events
  stripe_event_id text PRIMARY KEY, account_id text, type text NOT NULL, payload jsonb NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now(), processed_at timestamptz, attempts int DEFAULT 0);
```

RLS everywhere. We store **no** card data, no bank details, and no onboarding PII — those live at
Stripe. We keep identifiers, status, and amounts.

### Onboarding flow

1. `POST /api/v1/tenant/payments/connections` (entitlement + `payments.manage`) →
   create an Accounts v2 account with `configuration.merchant` requesting `card_payments`,
   `dashboard: "full"`, `fees_collector: "stripe"`, `losses_collector: "stripe"`, country and
   business profile prefilled from tenant data, idempotency-keyed by our connection id.
2. Return an **Account Session** client secret for the embedded `account_onboarding` component (the
   frontend renders it). Embedded onboarding is the default — we do not build API onboarding, which
   would force us to collect sensitive PII and build our own remediation flows.
3. Poll/receive `v2.core.account…` and `account.updated` webhooks → refresh `capabilities` and
   `requirements` → transition status.
4. `Active` only when `configuration.merchant.capabilities.card_payments.status == "active"`.
   Until then the tenant sees exactly which requirements are outstanding.
5. The tenant dashboard always renders the **`notification_banner`** embedded component, so evolving
   Stripe requirements surface to the tenant without us building a notification pipeline.
   `account_management` is offered for settings changes.

### Checkout flow (direct charges)

- `POST /api/v1/tenant/checkout` → validate connection `Active`, validate amount/currency against the
  connected account's country and supported currencies → create a Checkout Session **on the connected
  account** (`Stripe-Account: acct_…`), `mode: 'payment'`, no `payment_method_types` (dynamic payment
  methods; hardcoding `card` would lock out the local methods Japanese buyers expect),
  `integration_identifier` tag, locale from phase 06, success/cancel URLs on the tenant's own domain
  from phase 08.
- Fees: `payment_intent_data.application_fee_amount` populated only when the platform fee is
  configured (off by default in this release).
- Payment status is driven by **connected-account webhooks**, never by the browser redirect.
- Refunds: `POST /api/v1/tenant/payments/{id}/refund` (full/partial, reason, idempotent), permission
  `payments.refund`, audited.
- Disputes: read-only list plus a deep link to the tenant's Stripe Dashboard; we do not proxy
  evidence submission in this release.

### Webhooks

- Separate endpoint `/internal/webhooks/stripe/connect` with its own signing secret, distinct from
  the phase-04 platform endpoint; signature verified before parsing; Stripe IPs allow-listed.
- Events include the `account` field → map to tenant via `external_account_id`; **reject events for
  accounts we do not know** rather than creating rows.
- Same store-then-process, dedupe-by-event-id, out-of-order-guard pattern as phase 04.
- Handled: `account.updated`, `capability.updated`, `checkout.session.completed`,
  `payment_intent.{succeeded,payment_failed,canceled}`, `charge.refunded`,
  `charge.dispute.{created,closed}`, `payout.{paid,failed}`.

### Tax

Who collects tax on tenant transactions is a legal determination that follows merchant of record;
with direct charges that is the **connected account**. Implementation: when the tenant enables
automatic tax, set `automatic_tax.liability = { type: 'account', account: acct_id }`, and first
confirm the connected account's tax settings `status` is `active` and it has registrations —
otherwise no tax is collected and no error is returned. The UI states plainly that tax registration
and remittance are the tenant's responsibility, and we route the decision to their tax advisor.

### API surface (`api/payments.yaml`)

| Method | Path | Permission |
|---|---|---|
| GET | `/api/v1/tenant/payments/providers` | `payments.read` (catalog of connectable providers) |
| POST | `/api/v1/tenant/payments/connections` | `payments.manage` + entitlement |
| POST | `/api/v1/tenant/payments/connections/{id}/session` | `payments.manage` (Account Session secret) |
| GET | `/api/v1/tenant/payments/connections/{id}` | `payments.read` (status, requirements) |
| DELETE | `/api/v1/tenant/payments/connections/{id}` | `payments.manage` (disconnect, guarded) |
| POST | `/api/v1/tenant/checkout` | `payments.checkout` (or public, storefront-scoped, rate-limited) |
| GET | `/api/v1/tenant/payments` \| `/{id}` | `payments.read` |
| POST | `/api/v1/tenant/payments/{id}/refund` | `payments.refund` |
| GET | `/api/v1/tenant/payments/payouts` | `payments.read` |

## Work breakdown

1. Provider-agnostic domain + ports + provider catalog (entitlement-gated).
2. Stripe Connect adapter: Accounts v2 creation with the exact v2 dimensions above, Account Sessions,
   capability/requirement mapping, error taxonomy.
3. Connection lifecycle state machine + status sync job + `account.updated` handling.
4. Checkout creation on the connected account, currency/country validation, locale + domain wiring.
5. Connect webhook endpoint, processor, dedupe, mapping to tenants, DLQ.
6. Refunds, dispute read model, payout read model.
7. Application-fee configuration (per-tenant/plan), disabled by default, with the correct mechanism
   for direct charges when enabled.
8. Tax liability routing + the "connected account is not tax-registered" warning surface.
9. Disconnect flow: block while payments are in-flight or disputes are open; retain records for the
   statutory period (phase 05 retention override).
10. Runbooks: restricted account, failed payout, dispute handling, provider outage.

## Testing

- Stripe sandbox with test connected accounts: onboarding → restricted → active transitions.
- Capability gating: checkout is refused unless `card_payments.status == 'active'` (asserted against
  the v2 path, with a test that fails if anyone reintroduces `charges_enabled`).
- Direct-charge assertions: the charge exists on the connected account, funds are not on the platform
  balance, and no `application_fee_amount` is present while fees are disabled.
- Webhook tests: unknown account, duplicate delivery, out-of-order, bad signature, wrong endpoint
  secret (platform secret must not validate Connect events).
- Multi-tenant isolation: tenant A cannot create a checkout on tenant B's account (RLS + explicit
  account-ownership assertion in the adapter).
- Refund idempotency and partial-refund arithmetic in a zero-decimal currency (JPY).

## Security & compliance

- PCI scope stays SAQ-A: card data only ever touches Stripe-hosted surfaces and Stripe.js.
- Frontend CSP must allow `https://*.stripe.com` in `script-src`/`frame-src`/`connect-src` — fed
  through the shared `csp` package rather than ad-hoc per app.
- Connect webhook secret and RAKs are distinct per environment and stored in the secret manager;
  the RAK used for Connect has only the Connect-related permissions it needs.
- Onboarding never proxies PII through us; we store identifiers and status only.
- Every payment-affecting action (refund, disconnect, fee change) is audited with actor and reason.

## Observability

Metrics: `connect_onboarding_started/completed_total`, `connection_status_gauge{status}`,
`checkout_created_total{result}`, `payment_succeeded_total`, `payment_failure_total{code}`,
`connect_webhook_lag_seconds`, `dispute_open_gauge`. Alerts: connections stuck in `Onboarding` > 7
days, capability regressions to `Restricted`, webhook backlog, payout failures.

## Acceptance criteria

- [ ] A tenant connects Stripe through embedded onboarding and reaches `Active` with
      `configuration.merchant.capabilities.card_payments.status == 'active'`.
- [ ] A customer pays on the tenant's own domain and the funds land in the **tenant's** Stripe
      balance — verified in the sandbox, with no platform balance movement.
- [ ] The notification banner renders in the tenant dashboard and surfaces new requirements.
- [ ] Refunds work, are idempotent, and are correct in JPY and USD.
- [ ] No deprecated Connect pattern exists in the codebase (grep gate for `type: 'express'|'custom'|'standard'`,
      `charges_enabled`, `payouts_enabled`, `payment_method_types`).
- [ ] Adding a second provider requires only a new adapter (proven by a fake provider in tests).

## Risks

| Risk | Mitigation |
|---|---|
| Tenants stall mid-onboarding | Requirement summaries surfaced in-app, reminder notifications, embedded banner |
| Connected account gets restricted mid-trading | Status sync + immediate tenant alert + checkout blocked with a clear message |
| We accidentally become merchant of record | Direct charges only; a CI grep gate plus a sandbox assertion that funds never touch the platform balance |
| Currency/country mismatches at checkout | Validate against the connected account's country/currency support before session creation |
| Stripe API version drift | Pinned version; Connect webhook payloads replayed against handlers on upgrade |
