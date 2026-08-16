# ADR-0004: Platform billing on Stripe Billing, Checkout and the Customer Portal

- Status: accepted
- Deciders: platform team
- Date: 2026-08-18

## Context

Sanvi must collect its own subscription revenue before self-service signup goes live (phase 04,
target 0.5.0). The platform sells three plans (Starter / Professional / Enterprise), each mapping
to a set of feature entitlements that the access context enforces. Money flowing to *tenant
merchants* is out of scope here (phase 09).

## Decision

1. **Stripe Billing with Checkout Sessions (`mode: subscription`)** for signup. Checkout hosts the
   payment UI: SCA, trials, proration and dynamic payment methods come for free and no card data
   touches our servers (PCI SAQ-A).
2. **Customer Portal for self-service changes** (upgrades, downgrades, cancellation, payment-method
   updates). Portal actions return as webhooks like everything else.
3. **One Stripe Product per plan tier**; multiple Prices per product only for billing variants
   (monthly vs annual, currency). Invoice line items show the Product name — sharing one product
   across tiers would make them indistinguishable.
4. **Webhooks are authoritative for status.** Events are signature-verified, stored first
   (deduplicated by event id), acknowledged with 2xx immediately, and applied asynchronously by
   the worker with out-of-order guards. A daily reconciliation job re-derives local state from
   Stripe. API responses at request time are never used to grant access.
5. **Restricted API key** (`rk_`, least-privilege scopes, one per environment) from the secret
   manager; the API version is pinned per environment and upgrades are a tracked task with a
   replay test over stored payloads.
6. **Plan → entitlement mapping is data** (`billing.plan_entitlements`). Pricing changes are
   configuration, not deploys; entitlement recomputation on subscription state change is a pure,
   exhaustively table-tested function.
7. **Stripe Tax with a registration gate.** `automatic_tax` is only enabled where an active
   Stripe Tax registration exists; a daily ops check (and the CLI) fail loudly otherwise.
   Product tax codes are chosen from the canonical Tax Codes API and confirmed by the business
   owner — never invented or hardcoded.
8. **Usage-based billing is out of scope for 0.5.0.** If added later, evaluate Metronome (Stripe's
   platform for new usage-based billing) rather than Billing Meters.

## Consequences

- Subscription state changes are eventually consistent (webhook latency ≤ seconds; reconciliation
  bounds the tail). Acceptable: access lags payment by design, never leads it.
- Stripe outages degrade *changes*, not reads: entitlement resolution reads only local data.
- Dunning failures degrade to a documented state machine (grace expiry → downgrade → suspension)
  driven by a worker sweep, not by request paths.
- The `billing.webhook_events` table doubles as the upgrade-test corpus: replaying a month of
  stored payloads against new handler code is the API-version upgrade gate.
