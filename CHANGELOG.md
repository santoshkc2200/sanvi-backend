# Changelog

All notable changes to Sanvi backend. Generated from Conventional Commits;
phase sections group the slices a release ships. Versions follow the shared
roadmap's phase map (see `docs/roadmap.md`).

## 0.10.0 — Tenant Payment Providers (phases 09)

Merchant money: tenants connect Stripe via Connect (Accounts v2, embedded
onboarding), take direct charges on their **own** connected account, and
manage refunds, disputes and payouts — with Sanvi never the merchant of
record (funds never touch the platform balance; CI-gated invariant).

### Added

- **09.0 Foundations** — `payments` crate skeleton, schema + RLS, config,
  secrets, the `connect-invariants` CI gate and the provider-agnostic ports.
- **09.1 Provider domain** — connection aggregate, lifecycle state machine,
  provider registry & catalog endpoint; the fake provider adapter proving
  "a second provider is an adapter".
- **09.2 Stripe Connect onboarding** — Accounts v2 account creation keyed by
  idempotent connection ids, Account Sessions for embedded onboarding, error
  taxonomy (`payments/*` problem types).
- **09.3 Lifecycle & webhooks** — store-first Connect webhook transport
  (signature-first verification, dedupe by event id, retry budget → DLQ),
  webhook-driven status machine with out-of-order guards, reconciliation job.
- **09.4 Checkout on the connected account** — direct-charge checkout
  sessions (`Stripe-Account`), verified-domain redirect allow-list, per
  tenant/IP/customer throttling, payment webhooks recording money truth
  (`checkout.session.*`, `payment_intent.*`).
- **09.5 Payments, refunds & disputes** — payment/refund read models,
  portion-guarded refunds (JPY-correct), dispute projection with response
  deadlines, tenant-scoped CSV export.
- **09.6 Payouts, tax liability & fees** — payout projection
  (`payout.paid`/`payout.failed`), automatic-tax liability routed to the
  connected account behind a compliance preflight, application-fee
  configuration (off in 0.10.0).
- **09.7 Disconnect, hardening & release** — guarded disconnect
  (`DELETE /api/v1/tenant/payments/connections/{id}` refusing in-flight
  payments / open disputes / pending payouts by name), reconnect as a new
  connection row, statutory retention override for transaction records in
  the privacy data map, full metric set + alert catalogue +
  operator runbooks, the Stripe API version upgrade drill, threat model
  review (PCI scope confirmed SAQ-A), flags default on.

### Security

- Phase-02/03/06/07 hardening passes (review findings fixed).
- Connect restricted API key separated from the platform Billing key;
  webhook secret separation enforced at boot and by tests.

## 0.9.0 — Custom Domains, DNS & TLS (phase 08)

- Domain verification (DoH), registrar port (sandbox), ACME automation,
  TLS authorize gate, storefront probes, domain orders/purchase.

## 0.8.0 — Theming & Pluggable Templates (phase 07)

- Theme registry, versioned artifacts, yanking, tenant theme resolution.

## 0.7.0 — Internationalization & Localization (phase 06)

- Locale negotiation, ICU catalogs (en/ja), notification i18n, GPC/UOOM.

## 0.6.0 — Privacy & Compliance (phase 05)

- Jurisdiction profiles, data map, DSR export/erasure sagas, consent +
  opt-out ledger, retention sweep, legal holds, breach calculator.

## 0.5.0 — Platform Billing & Subscriptions (phase 04)

- Stripe Billing subscriptions, entitlement sync, dunning, Stripe Tax.

## 0.4.0 — Platform Admin, RBAC & Entitlements (phase 03)

- Roles/permissions authoring, feature catalog, entitlement gate.

## 0.3.0 — Identity & Access (phase 02)

- Ory Kratos/Keto integration, sessions, account linking, RBAC.

## 0.2.0 — Tenancy Core & Platform Kernel (phase 01)

- Tenant context, RLS isolation, outbox, jobs.

## 0.1.0 — Foundations (phase 00)

- Cargo workspace (actix-web, hexagonal), docker-compose, CI/CD,
  supply-chain security.
