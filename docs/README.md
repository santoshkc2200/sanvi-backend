# sanvi-backend — Documentation Index

Rust / actix-web modular monolith. Domain-Driven Design with hexagonal (ports & adapters)
architecture, PostgreSQL via compile-time-checked SQLx, Redis, docker-compose for local dev.

## How to read these docs

- [`architecture-overview.md`](architecture-overview.md) — the invariants every phase must respect.
- `phase-NN-<slug>/implementation-plan.md` — one plan per delivery phase. Each plan is
  self-contained: scope, decisions, deliverables, task breakdown, data model, tests, DoD.
- [`requirements.md`](requirements.md) — numbered `FR-*` / `NFR-*` requirements, from phase 09 onward.
  Tasks trace to these IDs.
- [`tasks/backlog.md`](tasks/backlog.md) — **start here to pick up work.** The status of every task,
  with the task specs in `tasks/phase-NN/`.
- [`runbooks/`](runbooks/) — operator procedures (impersonation, entitlement grants,
  audit chain incidents, billing/webhook repair, …).
- [`adr/`](adr/) — architecture decision records.
- The cross-project phase map lives in [`../../docs/roadmap.md`](../../docs/roadmap.md).

## Working the backlog

From phase 09 the delivery layers are: the roadmap (why the phase exists) → the phase
implementation plan (what and how) → the cross-track slice docs in `../../docs/phase-NN-*/` (order and
exit criteria per slice) → [`requirements.md`](requirements.md) (numbered requirements) →
[`tasks/`](tasks/backlog.md) (one agent-executable spec per slice). Every layer traces to the one
above it; `sdlc.py check` enforces it mechanically.

```bash
SDLC="python3 ~/.claude/skills/sdlc-planner/scripts/sdlc.py"
$SDLC check                          # traceability + status drift, run from this repo root
$SDLC status TASK-002 in-progress    # never hand-edit the status line and the backlog separately
```

Phases 00–08 predate this practice and stay documented by their implementation plans alone.

## Phases

| # | Phase | Version | Plan |
|---|-------|---------|------|
| 00 | Foundations & DevSecOps | 0.1.0 | [plan](phase-00-foundations-devsecops/implementation-plan.md) |
| 01 | Tenancy Core & Platform Kernel | 0.2.0 | [plan](phase-01-tenancy-core/implementation-plan.md) |
| 02 | Identity & Access (Ory Kratos + Keto) | 0.3.0 | [plan](phase-02-identity-access/implementation-plan.md) |
| 03 | Platform Admin, RBAC & Entitlements | 0.4.0 | [plan](phase-03-platform-admin-entitlements/implementation-plan.md) |
| 04 | Platform Billing & Subscriptions | 0.5.0 | [plan](phase-04-platform-billing/implementation-plan.md) |
| 05 | Privacy & Compliance (GDPR/APPI/US state laws) | 0.6.0 | [plan](phase-05-privacy-compliance/implementation-plan.md) |
| 06 | Internationalization & Localization | 0.7.0 | [plan](phase-06-i18n-l10n/implementation-plan.md) |
| 07 | Theming & Pluggable Templates | 0.8.0 | [plan](phase-07-theming/implementation-plan.md) |
| 08 | Custom Domains, DNS & TLS | 0.9.0 | [plan](phase-08-domains-tls/implementation-plan.md) |
| 09 | Tenant Payment Providers (Stripe Connect) | 0.10.0 | [plan](phase-09-tenant-payments/implementation-plan.md) |
| 10 | Advertising & Campaign Management | 0.11.0 | [plan](phase-10-advertising/implementation-plan.md) |
| 11 | Hardening, Observability & GA | 1.0.0 | [plan](phase-11-hardening-ga/implementation-plan.md) |

## Repository layout (target)

```
sanvi-backend/
├── Cargo.toml                  # cargo workspace
├── crates/
│   ├── kernel/                 # shared kernel: ids, errors, time, tenant ctx, outbox, pagination
│   ├── contexts/
│   │   ├── tenancy/
│   │   │   ├── src/domain/         # entities, value objects, events, errors, ports (traits)
│   │   │   ├── src/application/    # use cases, services, DTOs, command/query handlers
│   │   │   └── src/adapters/       # sqlx repos, redis, http handlers, external clients
│   │   ├── identity/  access/  platform_admin/  billing/  payments/
│   │   ├── privacy/   localization/ theming/  domains/  advertising/
│   │   ├── notifications/  audit/
│   └── platform/
│       ├── http/               # actix-web server, middleware, error mapping, OpenAPI
│       ├── persistence/        # pool, migrations runner, RLS helpers, tx manager
│       ├── messaging/          # outbox dispatcher, Redis Streams consumers
│       └── telemetry/          # tracing, OTel, metrics
├── apps/
│   ├── api/                    # composition root: wires adapters into use cases
│   ├── worker/                 # background jobs, event consumers, schedulers
│   └── cli/                    # migrate, seed, admin ops
├── api/                        # OpenAPI specs, one per bounded context
├── migrations/                 # SQL migrations, prefixed by context
├── configs/                    # base.toml + per-env overlays (secrets only from env/vault)
├── scripts/                    # dev, migrate, seed, lint, security helpers
├── docker-compose.yml
└── docs/                       # this folder
```

## Non-negotiables

1. `domain` depends on nothing but `kernel` and std/`serde`. No `sqlx`, no `actix`, no `reqwest`
   in a domain module — enforced in CI by dependency lint.
2. Every write path runs inside a transaction that also writes to the outbox when it emits events.
3. Every tenant-scoped table carries `tenant_id` and is protected by Row-Level Security.
4. Every external call (Stripe, Ory, registrar, ad platforms) sits behind a port with a fake used
   in tests, and a real adapter with timeouts, retries with jitter, and a circuit breaker.
5. Nothing user-facing is hardcoded in English; nothing tenant-specific is hardcoded at all.

## Versioning & release

Semantic versioning per [`../../docs/roadmap.md`](../../docs/roadmap.md). Conventional Commits drive
`CHANGELOG.md`. The HTTP API is versioned separately under `/api/v1`; breaking API changes require a
new path version and a deprecation window announced in the OpenAPI `deprecated` markers.
