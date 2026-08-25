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
