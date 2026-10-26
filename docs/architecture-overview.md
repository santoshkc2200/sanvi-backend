# Backend Architecture Overview

The rules below are phase-independent. Every implementation plan in this folder assumes them.

## 1. Modular monolith with bounded contexts

One deployable binary (`apps/api`) plus one worker binary (`apps/worker`), composed from independent
context crates. Contexts are the unit of ownership, of database schema, and of extraction — if a
context ever needs to become its own service, only its adapters change.

Context list and responsibility:

| Context | Owns |
|---------|------|
| `tenancy` | Tenants, tenant lifecycle/state, memberships, tenant settings, subdomain allocation |
| `identity` | Identities, credentials federation (Ory Kratos), sessions, account linking |
| `access` | Roles, permissions, policy authoring, permission checks (Ory Keto relation tuples) |
| `platform_admin` | Operator-facing tenant/feature/subscription administration, impersonation |
| `billing` | Platform plans, prices, subscriptions, invoices, entitlements, dunning |
| `payments` | Tenant-owned payment provider connections and merchant checkout |
| `privacy` | Jurisdiction profiles, consent & opt-out directives, data subject requests (export/erasure/appeals), retention, processing records |
| `localization` | Locales, translation catalogs, tenant locale settings, formatting policy |
| `theming` | Themes/templates registry, versions, tenant theme selection & overrides |
| `domains` | Custom domain claims, DNS verification, certificate lifecycle, registrar orders |
| `advertising` | Ad account connections, campaigns, creatives, spend/conversion metrics |
| `notifications` | Transactional email/SMS/webhook fan-out, templates, delivery status |
| `audit` | Append-only audit log, actor/action/target, tamper-evident chain |

## 2. Hexagonal layering

```
            ┌──────────── adapters (driving) ────────────┐
            │ actix handlers · CLI · event consumers      │
            └───────────────────┬────────────────────────┘
                                ▼
                       application (use cases)
                     commands · queries · DTOs
                                ▼
                    domain (pure): entities, VOs,
                    events, invariants, PORTS (traits)
                                ▲
            ┌───────────────────┴────────────────────────┐
            │ adapters (driven): sqlx repos · redis ·     │
            │ stripe · ory · dns · smtp · ad platforms    │
            └────────────────────────────────────────────┘
```

- **Domain**: pure Rust. Entities with private fields and invariant-enforcing constructors; value
  objects (`TenantId`, `Slug`, `Money`, `Locale`, `DomainName`); `thiserror` domain errors; domain
  events; ports as `#[async_trait]` traits.
- **Application**: one struct per use case with an `execute(cmd) -> Result<Output, AppError>`.
  Owns transactions, authorization checks, event publication, idempotency.
- **Adapters**: implement ports. Never leak `sqlx::Error` or `reqwest::Error` upward — map to domain
  or infrastructure errors at the boundary.

### 2.1 Accepted Layering Exceptions (Enforced by `scripts/check-layering.sh`)

The architecture gate enforces domain purity and dependency direction, with narrow, individually reviewed exceptions for existing technical debt:

- **Infrastructure types in domain port signatures**: Twelve contexts (`access`, `advertising`, `audit`, `billing`, `domains`, `identity`, `localization`, `payments`, `platform_admin`, `privacy`, `tenancy`, `theming`) import `sanvi_persistence` (`ScopedTx`, `PlatformScope`, or `PgPool`) in `src/domain/ports.rs` (or domain store/event definitions). These are permitted pending the transaction abstraction migration (ADR-002) to decouple port signatures from concrete database transaction handles.
- **Application-to-adapter references**: A narrow set of application services in `advertising`, `billing`, `domains`, `localization`, `payments`, `privacy`, and `theming` reference adapter types directly (Prometheus metrics instances, in-memory caches, adapter utility functions, and test fake providers). These are listed as exact-file exceptions pending port extraction.

## 3. Inter-context communication

- **Synchronous**: caller's application layer depends on a *port it defines itself*; the composition
  root wires in an adapter that calls the other context's application service. No context imports
  another context's domain types.
- **Asynchronous**: domain events written to the `outbox` table in the same transaction as the state
  change. A dispatcher in `apps/worker` makes retained rows available to PostgreSQL consumers,
  which are idempotent by `(consumer_group, event_id)`. This is the default for anything cross-context and non-critical-path.
- **Recovery**: published outbox rows remain the PostgreSQL replay source. Required consumers
  poll unprocessed/unparked events using PostgreSQL leases; publication never removes required
  history. Consumer failures are parked durably before acknowledgment. Retention and manual archival must preserve recovery and deduplication history; see `runbooks/event-recovery.md`.
- **Forbidden**: cross-schema SQL joins, shared tables, direct repository reuse across contexts.

PostgreSQL also persists recurring-job schedules and retry budgets. See
[ADR 0005](adr/0005-postgres-jobs-and-outbox.md) for delivery guarantees, future
broker adapters, and the initial worker handover procedure.

### 3.1 Accepted Context Ownership Exceptions (Enforced by `scripts/check-layering.sh`)

Bounded contexts must not import another context's domain or repository types. The following specific exceptions are documented and gated:

- **Privacy SPI (`PersonalDataProvider`)**: Every context's `src/adapters/privacy_provider.rs` implements `sanvi_privacy::domain::PersonalDataProvider` to support platform subject requests (export and erasure) pending port extraction to `sanvi-kernel`.
- **Audit Logging (`AuditWriter`)**: Security-critical use cases in `access`, `billing`, `payments`, and `platform_admin` import `sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter}` directly pending extraction of an audit port.
- **Platform Admin Composition**: `platform_admin` acts as a composite administration aggregate directly delegating tenant, membership, and entitlement state across `access`, `identity`, and `tenancy`.
- **Cross-context integrations**:
  - `billing/src/application/plans.rs` and `sync.rs` import `sanvi_access::domain` to synchronize subscription plan entitlements with access feature keys.
  - `advertising` imports `sanvi_payments::domain::CheckoutRepository` for conversion attribution and `sanvi_privacy::domain` for tracking consent directives.
  - `access` imports `sanvi_identity::domain` for membership synchronization and relation tuple rebuilding.

  The access projection adapter shares that identity boundary to load authoritative membership
  snapshots. PostgreSQL transaction-scoped advisory locks serialize reconciliation per aggregate;
  applied checkpoints commit only after idempotent Keto effects succeed. Tenant permission checks
  also require current active membership and current role definitions, independently of worker lag.

## 4. Persistence

- PostgreSQL, one database, **one schema per context** (`tenancy.`, `billing.`, …).
- SQLx with compile-time verification; `cargo sqlx prepare` output committed so CI builds offline.
- Migrations in `migrations/`, timestamp-prefixed and context-tagged:
  `20260901120000_billing_create_subscriptions.sql`. A CI lint asserts a migration only references
  its own schema. Every migration ships a reversible counterpart or a documented forward-fix.
- **Row-Level Security** on every tenant-scoped table; policies read `current_setting('sanvi.tenant_id')`.
  The transaction manager sets it with `SET LOCAL` at the start of each request transaction. The
  application role is `NOBYPASSRLS`; migrations use a separate owner role.
- No `SELECT *`. Explicit column lists so schema drift is a compile error.

### 4.1 Accepted Cross-Schema SQL Exceptions (Enforced by `scripts/check-layering.sh`)

Contexts must only query their own database schema. Two narrow exceptions exist for existing queries:

- `billing`: `crates/contexts/billing/src/adapters/repos.rs:764` joins `tenancy.tenants` in `list_dunning_due` to verify tenant status (`t.status = 'active'`) pending an asynchronous tenant state projection or port call.
- `localization`: `crates/contexts/localization/src/adapters/repos.rs:39` queries `identity.users` for user locale preference pending an identity client adapter.

## 5. Transport

- actix-web, routes mounted per context under `/api/v1/{context}`.
- Middleware stack (outermost → innermost): request id → tracing span → panic catch → CORS →
  security headers → rate limit → tenant resolution → authentication → tenant transaction scope.
- Errors serialized as RFC 9457 `application/problem+json`, with a stable `type` URI per error class
  and a `trace_id`. Domain error → HTTP status mapping lives in one place, `platform/http/error.rs`.
- OpenAPI generated from `utoipa` annotations; CI diffs the generated document against the committed
  `api/{context}.yaml` and fails on drift, so the spec is always the truth the frontend generates from.

## 6. Concurrency, resilience, performance

- Everything async on Tokio; blocking work goes to `spawn_blocking` or the worker.
- Every outbound HTTP client: connect + total timeouts, bounded retries with exponential backoff and
  full jitter (idempotent verbs only), circuit breaker, and a dedicated connection pool.
- Redis for: session cache, permission-check cache, rate limiting, idempotency keys, distributed
  locks (single-instance semantics, no Redlock), event streams, and short-TTL read caches with
  explicit invalidation on domain events.
- Caching rule: cache reads, never authorization decisions beyond a short TTL (≤ 30 s) with
  event-driven invalidation on role/permission change.

## 7. Security baseline

- Secrets only from environment/secret manager. `configs/*.toml` never contains a secret.
- All input validated at the application boundary (`validator` + newtype parsing). Reject-by-default.
- No PII in logs; a `Redacted<T>` wrapper and a tracing layer scrub known-sensitive fields.
- Every state-changing endpoint requires an explicit permission check — a test asserts no route is
  registered without one.
- Audit every privileged action (platform admin, impersonation, data export, billing changes).

## 8. Observability

- `tracing` with JSON output; OpenTelemetry OTLP export of traces and metrics
  to OpenObserve (its own OTLP/gRPC receiver — no collector in between).
- Every span carries `request_id`, `tenant_id`, `actor_id`, `route`, `context`.
- RED metrics per endpoint, USE metrics for pools, plus domain metrics per phase.
- Health endpoints: `/healthz` (liveness), `/readyz` (dependency checks), `/metrics` (Prometheus).
- The worker serves the same endpoints on private port 9091, with native worker metrics exposed
  for scraping. OTLP does not automatically export native Prometheus collectors. Unexpected task
  termination fails the worker and drains siblings instead of leaving a partially running process.

## 9. Testing pyramid

| Level | What | Tooling |
|-------|------|---------|
| Unit | Domain invariants, pure logic | `cargo test`, property tests via `proptest` |
| Application | Use cases against in-memory fake ports | fakes in `tests/support` |
| Integration | Repos + migrations + RLS | `sanvi_testsupport::test_db()` — a private database per test, cloned from a migrated template on a shared Postgres/Redis |
| Contract | Handlers vs OpenAPI, external APIs vs recorded fixtures | schema validation, `wiremock` |
| E2E | Cross-context journeys through HTTP | `apps/api` in docker-compose |
| Non-functional | Load, soak, chaos, security scans | `k6`, `cargo audit`, ZAP |

RLS is tested explicitly: a test asserts tenant A's connection cannot read tenant B's rows for
*every* tenant-scoped table (generated from the catalog, so new tables are covered automatically).

## 10. Billing (phase 04)

- **Stripe is the source of truth for money; webhooks are the source of truth for status.** The local
  `billing.subscriptions`/`billing.invoices` rows are a projection for *access* (entitlements).
  Webhook events are stored first (`billing.webhook_events`, PK on `stripe_event_id` = free
  deduplication), acknowledged immediately, and applied asynchronously by the worker. Out-of-order
  delivery is guarded by comparing event `created` against a per-provider-stream cursor (never the
  local processing timestamp); final state is reconciled daily by a job that re-reads live
  subscriptions from Stripe.
- **Plan → entitlement mapping is data, not code.** `billing.plans` + `billing.plan_entitlements`
  seed the three tiers; pricing changes never require a deploy. On every subscription state change
  the application rewrites the tenant's `access.entitlements` rows with `source = 'subscription'`
  and emits `billing.entitlements_recomputed` (the worker invalidates the entitlement cache).
  Operator overrides (`source = 'override'`) still win per the phase-03 precedence.
- **Dunning is a state machine on the subscription** (`collection_state`: ok → dunning →
  grace_expired). `invoice.payment_failed` starts the clock; the sweep expires grace windows,
  downgrades entitlements and suspends the tenant with the billing reason; `invoice.paid` restores
  everything — and only lifts suspensions *we* applied.
- **Stripe API rules**: pinned API version per environment, restricted key (`rk_`) with
  least-privilege scopes, `Idempotency-Key` on every write, never `payment_method_types`
  (dynamic payment methods), one Stripe Product per plan tier, Prices for billing variants only,
  `integration_identifier` with a random 8-letter suffix.
- **Stripe Tax gate**: `automatic_tax` is only enabled when an active registration exists (checked
  at Checkout creation and daily from the worker/CLI; enabling it without one silently collects
  nothing). Product tax codes come from the canonical Tax Codes API, confirmed by the business
  owner — never invented.
- No card data touches our servers (Checkout/Customer Portal are Stripe-hosted): PCI scope stays
  SAQ-A. Invoices contain personal data → the phase-05 data map registers them with a
  legal-retention rule that overrides erasure requests.
