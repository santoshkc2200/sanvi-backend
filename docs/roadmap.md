# Sanvi — Delivery Roadmap

Sanvi is a multi-tenant, highly configurable SaaS platform. It is delivered as two
repositories/workspaces:

- `sanvi-backend` — Rust (actix-web) modular monolith, DDD + hexagonal architecture.
- `sanvi-frontend` — pnpm + Turborepo monorepo (SvelteKit storefront & marketing, Svelte admin SPAs).

This file is the single index of phases and the only jointly-owned document. It lives in
`sanvi-backend/docs/shared/` because it needs one canonical copy and the backend already owns the
OpenAPI contract; the frontend reads it, and changes to it are reviewed by both tracks.

Each project holds its own detailed plan per phase:

- `sanvi-backend/docs/phase-NN-*/implementation-plan.md`
- `sanvi-frontend/docs/phase-NN-*/implementation-plan.md`

From phase 09 onward each project also carries the two layers below the plans — numbered
requirements and an executable task backlog, one task per slice per project:

- [`sanvi-backend/docs/requirements.md`](../requirements.md) · [`tasks/backlog.md`](../tasks/backlog.md)
- `sanvi-frontend/docs/requirements.md` · `sanvi-frontend/docs/tasks/backlog.md`

Every layer traces to the one above it: task → requirement → phase → roadmap. Task status is
recorded by hand in the task file and its backlog row, in the same commit, so the two never
disagree. Phases 00–08 predate this and remain documented by their implementation plans alone.

### Phases planned ahead of the code

A phase planned before it is built is first sliced into cross-track sub-phases — the order, the hard
blockers, and the exit criteria of each shippable slice, spanning both projects. That breakdown is
scaffolding, not a permanent layer: once a phase is decomposed into per-repo tasks, the slice
ordering moves into the tasks' `Depends on` / `Blocked by` lines and the shared breakdown is deleted.

- **Phase 09** — decomposed. See each repo's `docs/tasks/phase-09/`.
- **Phase 10** — decomposed. See each repo's `docs/tasks/phase-10/`.
- **Phase 11** (twelve slices) — decomposed. See each repo's `docs/tasks/phase-11/`.
- **Phases 12–15** — decomposed in the frontend only (`sanvi-frontend/docs/tasks/phase-12/` to
  `phase-15/`, TASK-033–056). The backend side of these phases is the product services
  (`accounting-service`, `ecommerce-service`) plus small platform-side changes that landed as
  plain commits; sanvi-backend has no `docs/tasks/phase-12` to `phase-15` and no backlog rows for
  them. Phase 14's one hard cross-track edge (the backend shopper path + OpenAPI mirror) landed in
  sanvi-backend and gated TASK-045.
- **Phase 15** — its gate was backend-repo work with no service change (landed in `0408a32` and
  `7562cf6`): define and grant the five `ecommerce.order.read`/`.manage`,
  `ecommerce.customer.read`/`.manage`/`.privacy` permissions (the delegated v2 token then carries
  them — no proxy route change; the `{tail}` GET/POST forward already serves the paths), and
  extend the `ecommerce-contract.yaml` mirror with the orders + customers surfaces.
- **Ecommerce platform adoption (2026-11-04)** — the backend now consumes the service's quote,
  shipping, discount, fulfillment, return and payment contracts: staff roles gain
  `ecommerce.order.discount`/`.fulfillment.record`/`.return.record`/`.payment.record`/`.refund`,
  guest checkout prices its hold with the service quote (the opaque fingerprint and totals are
  signed into the checkout claims; placement sends `expected_pricing_fingerprint` and maps the
  service's `ecommerce/checkout-price-changed` to re-approval instead of compensating cancels),
  the proxy forwards `Cache-Control: no-store`, the public scope lists active shipping methods,
  and the contract mirror publishes the quote/shipping/discount/fulfillment/return/payment
  surfaces. Still a gap: the provider-payment gateway adapter and the `sanvi.events.payments`
  stream (separate plan) and the storefront/admin UI (frontend repo).

## Phase map

| # | Phase | Version | Theme | Backend focus | Frontend focus |
|---|-------|---------|-------|---------------|----------------|
| 00 | Foundations & DevSecOps | 0.1.0 | Ground rules | Cargo workspace, hexagonal skeleton, docker-compose, CI/CD, supply-chain security | Turborepo + pnpm, shared configs, app shells, CI, CSP |
| 01 | Tenancy Core & Platform Kernel | 0.2.0 | Multi-tenancy | Tenant context, RLS isolation, outbox, jobs, config | Tenant resolution (subdomain), routing, API client |
| 02 | Identity & Access | 0.3.0 | Who are you | Ory Kratos + Keto adapters, session verification, account linking | Auth flows, social login, session store, guards |
| 03 | Platform Admin, RBAC & Entitlements | 0.4.0 | Control plane | Roles/permissions authoring, feature flags, entitlement gate | Platform admin console, tenant lifecycle UI |
| 04 | Platform Billing & Subscriptions | 0.5.0 | We get paid | Stripe Billing, plans, entitlement sync, dunning, Stripe Tax | Pricing, checkout, customer portal, billing UI |
| 05 | Privacy & Compliance | 0.6.0 | GDPR / APPI / US state laws | Jurisdiction profiles, data map, export, erasure, consent + opt-out, retention, audit | Consent & opt-out UI (incl. GPC), privacy centre, cookie policy |
| 06 | Internationalization & Localization | 0.7.0 | en + ja | Locale negotiation, ICU catalogs, email/notification i18n | Paraglide catalogs, locale routing, CJK typography |
| 07 | Theming & Pluggable Templates | 0.8.0 | Look & feel | Theme registry, versioned artifacts, tenant theme resolution | Token runtime, template/layout system, theme editor |
| 08 | Custom Domains, DNS & TLS | 0.9.0 | Own your URL | Domain verification, registrar port, ACME automation | Domain wizard (connect/purchase), status UI |
| 09 | Tenant Payment Providers | 0.10.0 | Merchant money | Stripe Connect (Accounts v2, direct charges), provider abstraction | Connect onboarding, embedded components, checkout |
| 10 | Advertising & Campaigns | 0.11.0 | Growth | Google/Meta Ads adapters, conversion pipeline, metrics store | Campaign manager, ROAS dashboard, spend tracking |
| 11 | Hardening, Observability & GA | 1.0.0 | Ship it | SLOs, tracing, load/DR tests, pen-test remediation | Performance budgets, a11y, e2e suite, error tracking |
| 12 | Accounting Connect | 1.1.0 | Books | Accounting service (books, journal entries), freee OAuth connector | Accounting admin surfaces, connect flow |
| 13 | Ecommerce Admin | 1.2.0 | Catalog | `ecommerce-service` (stores, products/variants/prices, stock), entitlement gate, service-key proxy | Ecommerce admin surfaces (stores, products, stock) |
| 14 | Ecommerce Shopper Storefront | 1.3.0 | Shoppers buy | Public scope: host-scoped catalog read, guest checkout orchestration, purchase credentials, OpenAPI mirror | `apps/ecommerce` store: catalog, guest checkout, hosted-redirect payment, credential-scoped order status |
| 15 | Ecommerce Orders & Customers | 1.4.0 | Order ops | The five `ecommerce.order.*`/`ecommerce.customer.*` permissions granted through the delegated token + the orders/customers OpenAPI mirror (no proxy change — the `{tail}` forward already carries the paths) | Orders list/snapshot detail/cancel with typed refusals; customer records, add-and-archive addresses, export/erase (staff-initiated DSR) |

## Sequencing rules

1. **Phases are ordered by dependency, not by value.** Everything after 01 assumes a resolvable
   tenant context; everything after 02 assumes an authenticated subject and a permission check.
2. **Cross-cutting concerns are contracted early, delivered later.** i18n (06) and theming (07)
   have *lint-enforced contracts from phase 00* (no hardcoded user-facing strings, no hardcoded
   colors/fonts) so those phases are implementations, not retrofits.
3. **Each phase ends releasable.** A phase is done when it is deployed to staging behind a flag,
   documented, and covered by tests — not when the code merges.
4. **Jurisdictions are configuration.** The platform targets the EU/UK, Japan, and the United
   States. Privacy rules that differ by territory (consent vs. notice-and-opt-out, deadlines, appeal
   rights, breach thresholds) live in jurisdiction profiles seeded in phase 05, never in branching
   code — so a new US state law is a data change plus a test.
5. **Parallelism.** Backend and frontend tracks of the same phase are designed to run concurrently
   against the OpenAPI contract, which is written and merged first (contract-first).

## Conventions shared by both projects

- **Versioning**: semantic versioning. Phase completion bumps the minor version until GA (1.0.0).
  Breaking public API changes bump major post-GA. Every phase tags `vX.Y.0` and ships a changelog
  entry generated from Conventional Commits.
- **Contract-first**: the OpenAPI documents in `sanvi-backend/api/{context}.yaml` are the contract.
  The frontend consumes a generated, type-safe client; CI fails if handlers and spec diverge.
- **Bounded contexts** (stable list, both sides): `tenancy`, `identity`, `access` (RBAC),
  `platform-admin`, `billing`, `payments`, `privacy`, `localization`, `theming`, `domains`,
  `advertising`, `notifications`, `audit`.
- **No cross-context database access.** Contexts talk over application ports (in-process) or domain
  events (outbox → stream). This is what keeps the monolith extractable into services later.
- **Definition of Done** (every phase): tests green, OpenAPI updated, migrations reversible,
  observability (logs/metrics/traces) wired, threat model reviewed, docs updated, feature flag
  defined, rollback plan written.

## Reusable frontend packages already present

| Package | Status | First phase that uses it |
|---------|--------|--------------------------|
| `design-tokens` | Present (DTCG → CSS/TS via Style Dictionary) | 00 (rescope), 07 (theme engine) |
| `csp` | Present (Content-Security-Policy builder) | 00 (all apps), 09 (Stripe origins) |
| `billing-elements` | Present (Stripe Elements + money helpers) | 04 (platform billing), 09 (tenant checkout) |
| `course-media` | Present (upload, HLS, captions, images) | Not required by phases 00–11; keep building, adopt when a product context needs media |

Both `design-tokens` and `csp` depend on `@hitox/ts-config` and `@hitox/test-config` workspace
packages that do not exist yet — creating them is phase 00 work.

## Delivery log

| Version | Phase | Tagged | Notes |
|---------|-------|--------|-------|
| 0.10.0 | 09 — Tenant Payment Providers | backend track complete; tag `v0.10.0` lands when both tracks' 09.7 are done (cross-repo gate) | Stripe Connect (Accounts v2), direct-charge checkout, refunds/disputes/payouts, guarded disconnect, statutory retention override for transaction records, payments observability + runbooks |
| 1.1.0 | 12 — Accounting Connect | both tracks complete; the `v1.1.0-alpha.N`/`v1.1.0` tags the task headers named were never cut in git (their closeout notes recorded them as commit-time steps that did not happen) | freee OAuth connector and accounting admin surfaces |
| 1.2.0 | 13 — Ecommerce Admin | frontend complete (TASK-039–044); `v1.2.0` recorded as pending by TASK-044's execution notes and not yet cut | `ecommerce-service` admin surface through the service-key proxy: stores, products/variants/prices, stock overview; entitlement gate |
| 1.3.0 | 14 — Ecommerce Shopper Storefront | frontend complete (TASK-045–050); `v1.3.0-alpha.1..6` and `v1.3.0` cut by TASK-050 on the phase commits | the shopper path: public catalog read, guest checkout orchestration, purchase credentials, OpenAPI mirror; `apps/ecommerce` becomes the real tenant store |
| 1.4.0 | 15 — Ecommerce Orders & Customers | both tracks complete (backend gate in `0408a32` + the mirror; frontend TASK-051–056); `v1.4.0-alpha.1..6` and `v1.4.0` cut on the phase commits | the staff side of the paper trail: orders list/snapshot detail/cancel with typed refusals; tenant customer records, add-and-archive addresses, staff-initiated export/erase (DSR); the orders/customers mirror this phase's backend half |
