# Phase 04 — Platform Billing & Subscriptions (backend)

**Target version:** 0.5.0
**Depends on:** phases 01, 02, 03
**Unlocks:** self-service tenant signup; entitlements now have a commercial source

## Goal

Tenants subscribe to Sanvi and pay Sanvi with Stripe. Plan → entitlement mapping is data, not code,
so pricing changes do not require a deploy. Stripe is the source of truth for money; our database is
the source of truth for *access*, kept in sync by webhooks that are idempotent and replay-safe.

**This phase is about the platform's own revenue.** Money flowing to *tenant merchants* is phase 09.

## Scope

**In**
- `billing` context: plans, prices, subscriptions, invoices, payment methods, dunning, trials,
  proration, cancellations, plan → entitlement mapping, usage counters for quota-priced features.
- Stripe integration: Checkout Sessions (`mode: subscription`), Customer Portal, webhooks.
- Stripe Tax setup and the registration-gating rule.

**Out**
- Stripe Connect / tenant merchant payments (phase 09).
- Usage-based *rated* billing beyond simple quota overage — see the decision table.

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Checkout | Stripe **Checkout Sessions**, `mode: 'subscription'` | Handles SCA, trials, proration, dynamic payment methods, and localisation for us |
| Payment methods | **Never** pass `payment_method_types` | Dynamic payment methods are configured in the Dashboard and maximise conversion; hardcoding `card` locks out others |
| Self-service management | Stripe **Customer Portal** for upgrades/downgrades/cancel/payment-method updates | Weeks of UI we do not have to build or maintain |
| Product catalog | **One Stripe Product per plan tier** (Starter / Professional / Enterprise), multiple Prices per product only for billing variants (monthly vs annual, currency) | Invoice and Checkout line items show the Product name; sharing one Product across tiers makes them indistinguishable |
| Prices | `Price` objects only — never the deprecated `plan` object | Current API |
| Tax | Stripe Tax with `automatic_tax: { enabled: true }`, **only after an active registration exists** per jurisdiction | Enabling it without a registration silently collects nothing and returns no error — the most common Stripe Tax mistake |
| Product tax code | Chosen from the canonical Tax Codes API list and confirmed by the business owner; never invented or hardcoded from memory | Legal correctness is not ours to guess; generic ESS is too broad for US state taxability |
| Usage-based billing | Out of scope for 0.5.0. If it is added later, evaluate **Metronome** rather than Billing Meters for a new integration | Metronome is Stripe's platform for new UBB (metering, rating, credits) |
| Source of truth | Stripe for money; local projection for entitlements; **webhooks are the truth for status**, not API responses at request time | Sync responses can race; webhook events are authoritative |
| API keys | **Restricted API key** (`rk_`) with least-privilege scopes, from the secret manager, separate key per environment | A compromised RAK does far less damage than an `sk_` |
| API version | Pin the account and SDK to a single, current API version; upgrades are a tracked task with a replay test | Silent version drift breaks webhook payload assumptions |
| Idempotency | `Idempotency-Key` on every Stripe write, derived from our command id | Safe retries |

## Domain model

```rust
pub struct Plan { id: PlanId, key: String, stripe_product_id: String, tier: PlanTier,
                  entitlements: Vec<EntitlementGrant>, visibility: Visibility, sort_order: i32 }
pub struct PlanPrice { id, plan_id, stripe_price_id, currency: Currency, interval: Interval,
                       unit_amount_minor: i64, trial_days: Option<u16>, active: bool }
pub struct Subscription { id, tenant_id, plan_id, price_id, stripe_subscription_id,
                          status: SubscriptionStatus, current_period: Period,
                          cancel_at_period_end: bool, trial_end: Option<Timestamp>,
                          collection_state: CollectionState }
pub struct InvoiceRef { id, tenant_id, stripe_invoice_id, number, status, total_minor,
                        currency, hosted_url, pdf_url, period }
```

`SubscriptionStatus` mirrors Stripe (`trialing, active, past_due, unpaid, canceled, incomplete,
incomplete_expired, paused`) — deliberately not re-invented, so webhook mapping is total.

Events: `SubscriptionStarted`, `SubscriptionRenewed`, `SubscriptionPlanChanged`,
`SubscriptionPastDue`, `SubscriptionCanceled`, `TrialEnding`, `InvoicePaid`, `InvoicePaymentFailed`,
`EntitlementsRecomputed`.

## Plan → entitlement mapping

```sql
CREATE SCHEMA billing;
CREATE TABLE billing.plans (id uuid PRIMARY KEY, key text UNIQUE NOT NULL, tier text NOT NULL,
  stripe_product_id text NOT NULL UNIQUE, visibility text NOT NULL, sort_order int NOT NULL);
CREATE TABLE billing.plan_entitlements (plan_id uuid, feature_key text, enabled boolean,
  "limit" bigint, PRIMARY KEY (plan_id, feature_key));
CREATE TABLE billing.plan_prices (id uuid PRIMARY KEY, plan_id uuid NOT NULL,
  stripe_price_id text NOT NULL UNIQUE, currency text NOT NULL, interval text NOT NULL,
  unit_amount_minor bigint NOT NULL, trial_days int, active boolean NOT NULL DEFAULT true);
CREATE TABLE billing.subscriptions (id uuid PRIMARY KEY, tenant_id uuid NOT NULL UNIQUE,
  plan_id uuid NOT NULL, price_id uuid NOT NULL, stripe_customer_id text NOT NULL,
  stripe_subscription_id text NOT NULL UNIQUE, status text NOT NULL,
  current_period_start timestamptz, current_period_end timestamptz,
  cancel_at_period_end boolean NOT NULL DEFAULT false, trial_end timestamptz,
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE billing.invoices (...);
CREATE TABLE billing.webhook_events (stripe_event_id text PRIMARY KEY, type text NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now(), processed_at timestamptz,
  api_version text, payload jsonb NOT NULL, attempts int NOT NULL DEFAULT 0, error text);
```

On every subscription state change the application recomputes entitlements for the tenant from
`plan_entitlements` and writes them with `source = 'subscription'`, then emits
`EntitlementsRecomputed` so phase 03's cache invalidates. Operator overrides (source `override`)
still win — that precedence is already defined in phase 03.

## Lifecycle flows

**Subscribe**
1. `POST /api/v1/tenant/billing/checkout-session` → ensure a Stripe Customer for the tenant (stored,
   idempotent) → create Checkout Session (`mode: subscription`, `line_items: [{price, quantity}]`,
   `subscription_data.trial_period_days`, `automatic_tax: {enabled}` *if registrations exist*,
   `tax_id_collection` for B2B, `client_reference_id = tenant_id`, `integration_identifier` with a
   random 8-letter suffix for Dashboard comparison, locale from phase 06) → return the URL.
2. Frontend redirects; Stripe hosts the payment.
3. `checkout.session.completed` + `customer.subscription.created` webhooks create the local
   subscription and recompute entitlements. **We do not grant access from the redirect return.**

**Manage** — `POST /api/v1/tenant/billing/portal-session` returns a Customer Portal URL configured
with the allowed plan-change set; portal actions arrive back as webhooks.

**Dunning** — `invoice.payment_failed` → mark `past_due`, notify, start the grace window (config,
default 14 days) → grace expiry → downgrade entitlements to the free/limited set and suspend the
tenant per phase 01's state machine. `invoice.paid` restores immediately.

**Cancel** — default `cancel_at_period_end`; entitlements persist until `current_period_end`, then a
scheduled job applies the downgrade (scheduled from the event, not from a polling loop).

## Webhook handling

- Endpoint `/internal/webhooks/stripe/platform`, **signature verified** with the endpoint secret
  before any parsing, plus Stripe IP allow-listing for defence in depth.
- Store the raw event first (`billing.webhook_events`, PK on `stripe_event_id` = free deduplication),
  return `2xx` immediately, process asynchronously in `apps/worker`.
- Out-of-order tolerance: subscription and invoice handlers compare event `created` against their
  own Stripe event cursors on the local row and ignore stale updates; local `updated_at` is never an
  ordering signal. Final state is reconciled by a daily job that re-reads the subscription.
- Unknown event types are stored and ignored, not errors.
- Handled: `checkout.session.completed`, `customer.subscription.{created,updated,deleted,paused,resumed}`,
  `invoice.{paid,payment_failed,finalized,upcoming}`, `customer.updated`,
  `payment_method.attached/detached`, `charge.dispute.created`.

## API surface (`api/billing.yaml`)

| Method | Path | Permission |
|---|---|---|
| GET | `/api/v1/public/plans` | none (public pricing page, localized) |
| POST | `/api/v1/tenant/billing/checkout-session` | `billing.subscription.create` |
| POST | `/api/v1/tenant/billing/portal-session` | `billing.subscription.update` |
| GET | `/api/v1/tenant/billing/subscription` | `billing.subscription.read` |
| GET | `/api/v1/tenant/billing/invoices` | `billing.invoice.read` |
| GET/POST/PATCH | `/api/v1/platform/plans` (+ prices, entitlement map) | `platform.plan.write` |
| POST | `/api/v1/platform/tenants/{id}/subscription/override` | `platform.subscription.override` |

## Work breakdown

1. Stripe account/sandbox setup, RAK creation with minimal scopes, secret-manager wiring, CLI webhook
   forwarding for local dev.
2. `billing` domain + schema + plan/price/entitlement mapping CRUD (platform-scoped).
3. Stripe client adapter: typed wrapper, timeouts, retries, idempotency keys, error taxonomy.
4. Customer provisioning, Checkout Session creation, Portal session creation.
5. Webhook receiver + async processor + dedupe + out-of-order guard + DLQ.
6. Entitlement recomputation service and its event emission.
7. Dunning state machine, grace windows, notification templates, tenant suspension hook.
8. Stripe Tax: registration check before enabling `automatic_tax`; a startup/ops check that surfaces
   "automatic tax enabled but no active registration" as a loud warning; tax-ID collection for B2B.
9. Reconciliation job (Stripe → local) + drift metric; invoice mirroring for the tenant UI.
10. Runbooks: failed webhook replay, refund, plan migration, tax registration change.

## Testing

- `stripe-mock`/recorded fixtures for unit and application tests; a sandbox account for the nightly
  end-to-end suite (subscribe → renew via test clock → fail payment → dunning → recover → cancel).
- **Stripe test clocks** for trial end, renewal, and dunning timelines without waiting.
- Webhook tests: duplicate delivery, out-of-order delivery, invalid signature, unknown type, replay
  after outage.
- Entitlement recomputation table test: every plan × status combination yields the expected gate set.
- Failure injection: Stripe 500s and timeouts during checkout creation must not leave a half-state.

## Security & compliance

- No card data ever touches our servers (Checkout/Portal are Stripe-hosted); PCI scope stays SAQ-A.
- Webhook secrets and RAKs in the secret manager; pre-commit hook blocks `sk_`/`rk_` strings.
- Billing actions are audited (phase 03), including plan overrides and manual entitlement grants.
- Invoices contain personal data → registered in the phase 05 data map with a legal-retention rule
  that **overrides erasure requests** for the statutory period.

## Observability

Metrics: `stripe_request_duration{op}`, `stripe_error_total{code}`, `webhook_lag_seconds`,
`webhook_unprocessed_gauge`, `subscription_status_gauge{status}`, `mrr_gauge{currency}`,
`dunning_active_gauge`.
Alerts: unprocessed webhooks > 5 min, signature failures > 0, reconciliation drift > 0.

## Acceptance criteria

- [ ] A tenant subscribes via Checkout and the matching entitlements are live within 10 s of the
      webhook, not of the redirect.
- [ ] Upgrade/downgrade through the Customer Portal changes entitlements correctly, with proration
      handled by Stripe.
- [ ] A failed payment moves the tenant through dunning and, after grace, to the limited entitlement
      set; recovery restores it.
- [ ] Replaying a month of webhook events changes nothing (idempotent).
- [ ] `automatic_tax` is only enabled where an active registration exists, and the ops check proves it.
- [ ] No Stripe secret exists outside the secret manager (scan is green).

## Risks

| Risk | Mitigation |
|---|---|
| Webhook outage silently freezes entitlements | Reconciliation job + unprocessed-webhook alert + manual replay runbook |
| Tax enabled without registration → collecting nothing | Explicit pre-flight check and startup warning; documented in the go-live checklist |
| Pricing model change requires code | Plans/prices/entitlement map are data with a platform-admin UI |
| Stripe API version upgrade breaks handlers | Version pinned; upgrade branch replays stored `webhook_events` payloads against new handlers |
| Local/Stripe divergence during incidents | Stripe is authoritative for money; a documented one-way repair job re-derives local state |
