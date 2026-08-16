# Billing runbook

Operator procedures for the billing context (phase 04). Stripes are the source of truth for money;
local rows are a projection for access, repaired by reconciliation. **Never edit
`billing.subscriptions` by hand** — repair via the documented paths below.

Prerequisites: `just` (or the underlying `cargo run -p sanvi-cli` commands) and access to the
environment's secret manager.

## 1. Local development setup

- `docker compose --profile billing up -d stripe-mock` starts a local stand-in for the Stripe API
  (no real charges, no real keys).
- Point the API at it:
  `SANVI__BILLING__STRIPE_API_BASE=http://localhost:12111 cargo run -p sanvi-api`.
- Webhook forwarding uses the real Stripe CLI against the test-mode account:
  `docker compose --profile billing run --rm stripe-cli listen --forward-to host.docker.internal:8080/internal/webhooks/stripe/platform`
  (needs `docker login` and a Stripe API key in the CLI).
- Real Stripe: set `SANVI__BILLING__STRIPE_API_KEY` (restricted key, `rk_`) and
  `SANVI__BILLING__STRIPE_WEBHOOK_SECRET` (`whsec_`). Never commit either.

## 2. Plans, prices, entitlement maps

Plans are seeded by migration (Starter / Professional / Enterprise). The `stripe_product_id` values
are placeholders — replace them with the real Products from the Dashboard before go-live:

1. Create the Product in the Stripe Dashboard (one per plan tier).
2. Create its Prices (monthly/annual per currency) and note the price ids.
3. Patch the plan: `curl -X PATCH $API/api/v1/platform/plans/{plan_id} -d '{...}'` (or the API),
   then `POST /api/v1/platform/plans/{plan_id}/prices` for each price, and finally
   `PUT /api/v1/platform/plans/{plan_id}/entitlements` with the feature map.
4. Review: `just billing-plans`.

Pricing changes never require a deploy; tenants get the new map on their next entitlement
recompute.

## 3. Failed webhook replay

Webhooks are store-first: `billing.webhook_events` keeps every received payload. The worker
retries each event up to 5 times; rows that exhaust retries become dead-letter.

1. See the backlog: `just billing-webhooks`.
2. Inspect failures: `SELECT stripe_event_id, type, attempts, error FROM billing.webhook_events
   WHERE processed_at IS NULL AND attempts >= 5;` (read-only, migrator role).
3. After fixing the root cause, requeue the rows by resetting attempts — **only** via a tracked
   SQL script under the migrator role, e.g.:
   `UPDATE billing.webhook_events SET attempts = 0, error = NULL WHERE processed_at IS NULL;`
   The dispatcher picks them up within `scheduler.webhook_dispatch_interval_secs` (10 s).
4. Outage repair beyond individual events: `just billing-reconcile` re-derives every live
   subscription from Stripe; it is safe to run any time (idempotent, out-of-order guarded).

## 4. Refunds

Refunds happen in Stripe (Dashboard or API). `invoice.paid`-adjacent webhooks keep local invoice
rows consistent; entitlements are unaffected by refunds of already-paid invoices. If the refund
belongs to a subscription the tenant should not keep (e.g. dispute upheld), cancel the
subscription in Stripe — the `customer.subscription.deleted` webhook clears entitlements at
period end. Never grant or revoke access by editing local rows.

## 5. Plan migration (mass pricing change)

1. Update the entitlement maps via the platform API (`PUT .../entitlements`).
2. Recompute affected tenants' entitlements by touching their subscriptions: the daily
   reconciliation rewrites them; to force immediately, run `just billing-reconcile`.
3. If the plan *keys* change (new tiers), keep the old plans `hidden` and create new ones; never
   repoint a `stripe_product_id` to a different product while live subscriptions reference its
   prices.

## 6. Tax registration change

`automatic_tax` must only be enabled while an active Stripe Tax registration exists. Registrations
are managed in the Stripe Dashboard (or the Tax Registrations API) — recording one there does not
register the business with the tax authority; the business owner owns that step.

- After adding/expiring registrations: `just billing-tax-check` confirms the gate's view.
- Enabling automatic tax: set `billing.automatic_tax_enabled = true` **only after** the check
  passes; redeploy. The daily `billing.tax_check` job and the Checkout pre-flight both fail loudly
  (and page) if the invariant breaks later.

## 7. Dunning incidents

- Grace windows run from `collection_started_at`; `just billing-webhooks` + the
  `dunning_active_gauge` metric show how many subscriptions are inside the window.
- A sweep that fails to suspend a tenant retries on the next pass (entitlements are already
  downgraded — fail-closed for revenue).
- Recovery is automatic on `invoice.paid`. A tenant suspended for billing can also be resumed
  manually via the tenancy lifecycle (reason `billing`), but only payment should end dunning.

## 8. Stripe API version upgrades

The API version is pinned per environment (`billing.stripe_api_version`). To upgrade:

1. Create a branch changing the pin.
2. Replay a month of stored `billing.webhook_events` payloads against the new handlers (reset
   `processed_at`/`attempts` on a staging copy — see section 3).
3. Run the nightly sandbox e2e suite (subscribe → renew → fail → dunning → recover → cancel).
4. Merge, deploy, then update the webhook endpoint's API version in the Dashboard.

## 9. Secrets rotation

Rotate `rk_` keys and `whsec_` secrets in the secret manager, then redeploy. Webhook signatures
are verified with the configured secret — brief overlap windows are unavoidable; plan rotation
during low-traffic hours and keep the old secret active until the new one is live everywhere.

## 10. Alerting rules (expected)

- `webhook_unprocessed_gauge > 0` for more than 5 minutes → page (webhooks are stuck).
- `webhook_dead_letter_gauge > 0` → page (events exhausting retries).
- `reconciliation_drift_total` rate > 0 → investigate (Stripe/local divergence).
- `dunning_active_gauge` sudden step → investigate (mass payment failures).
- Tax check error log (daily job) → page (automatic_tax without registrations).
