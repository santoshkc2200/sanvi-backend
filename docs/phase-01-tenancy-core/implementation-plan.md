# Phase 01 — Tenancy Core & Platform Kernel (backend)

**Target version:** 0.2.0
**Depends on:** phase 00
**Unlocks:** every tenant-scoped feature (02–11)

## Goal

Make "which tenant is this?" a solved, unforgeable, always-present fact. After this phase every
request carries a resolved `TenantContext`, every tenant-scoped row is isolated by the database
itself, and the async backbone (outbox → stream → consumers) that later contexts rely on is running.

## Scope

**In**
- `tenancy` bounded context: tenant aggregate, lifecycle, settings, subdomain allocation, membership
  placeholders (filled by phase 02).
- Tenant resolution middleware (subdomain, custom-domain lookup hook, explicit header for
  server-to-server, JWT/session claim once phase 02 lands).
- Postgres RLS enforcement and the transaction scoping that drives it.
- Outbox dispatcher + Redis Streams consumers + job scheduler in `apps/worker`.
- Tenant-aware caching, rate limiting, and idempotency.

**Out**
- Who the user is and what they may do (phase 02/03).
- Tenant self-service signup UI flows (frontend phase 03) — API exists, an operator creates tenants
  by CLI until then.

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Isolation model | Shared database, shared schema, `tenant_id` column + RLS | Cheapest to operate at this stage; RLS makes isolation a DB guarantee, not a code habit. Reviewed at scale-out (ADR 0002 records the exit path to schema- or cluster-per-tenant for enterprise tiers) |
| Tenant id | UUIDv7 `TenantId` newtype | Time-sortable, index-friendly, opaque externally |
| Resolution order | explicit internal header (signed) → session claim → custom domain → subdomain → none | Most specific wins; `none` is only legal on platform/public routes |
| Slug/subdomain | `[a-z0-9]([a-z0-9-]{1,61}[a-z0-9])?`, reserved-word blocklist, immutable after creation | DNS-safe, avoids phishing/impersonation via `www`, `api`, `admin`, `mail`, … |
| Tenant state machine | `provisioning → active → suspended → archived → purged` | Billing (04) and privacy (05) hook into these transitions |
| Cross-tenant queries | Only via an explicit `PlatformScope` guard, audited | Platform admin needs it; nothing else does |

## Domain model

```rust
// domain/tenant.rs
pub struct Tenant {
    id: TenantId,
    slug: TenantSlug,          // immutable, unique, DNS-safe
    display_name: TenantName,
    status: TenantStatus,      // Provisioning|Active|Suspended|Archived|Purged
    region: DataRegion,        // eu|jp|us — drives residency decisions in phase 05
    default_locale: Locale,    // phase 06 consumes
    created_at: Timestamp,
    suspended_reason: Option<SuspensionReason>,
}
```

Invariants: slug immutable and globally unique; only `Active` tenants serve traffic; `Suspended`
allows admin/billing access only; `Archived` is read-only and schedules purge; transitions are
explicit methods emitting events.

Events: `TenantProvisioned`, `TenantActivated`, `TenantSuspended`, `TenantResumed`,
`TenantArchived`, `TenantPurgeRequested`, `TenantSettingsChanged`, `TenantSlugReserved`.

Ports: `TenantRepository`, `TenantSettingsRepository`, `SlugReservationPort`, `EventPublisher`,
`Clock`, `IdGenerator`.

## Deliverables

### 1. Schema and RLS

```sql
CREATE SCHEMA tenancy;

CREATE TABLE tenancy.tenants (
  id             uuid PRIMARY KEY,
  slug           citext      NOT NULL UNIQUE,
  display_name   text        NOT NULL,
  status         text        NOT NULL,
  region         text        NOT NULL,
  default_locale text        NOT NULL DEFAULT 'en',
  created_at     timestamptz NOT NULL DEFAULT now(),
  updated_at     timestamptz NOT NULL DEFAULT now(),
  archived_at    timestamptz
);

CREATE TABLE tenancy.tenant_settings (
  tenant_id  uuid NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  key        text NOT NULL,
  value      jsonb NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, key)
);

ALTER TABLE tenancy.tenant_settings ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenancy.tenant_settings FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenancy.tenant_settings
  USING (tenant_id = current_setting('sanvi.tenant_id', true)::uuid);
```

A shared migration helper `platform.enable_tenant_rls('schema.table')` applies the standard policy so
every later context gets identical semantics with one line.

### 2. Tenant context propagation

- `TenantContext { tenant_id, slug, status, region, default_locale, resolution_source }` inserted by
  middleware into request extensions and into the tracing span.
- `TxManager::begin_for(tenant)` issues `SET LOCAL sanvi.tenant_id = $1` (and
  `SET LOCAL ROLE sanvi_app` where applicable) before any statement runs. **Repositories can only be
  constructed from a scoped transaction**, so forgetting the scope is a type error rather than a leak.
- `PlatformScope::begin_unscoped(reason, actor)` is the single audited escape hatch.
- Cache keys are namespaced `sanvi:{tenant_id}:{context}:{key}`; a `TenantCache` wrapper makes the
  prefix impossible to omit.

### 3. Resolution middleware

Order and failure modes:

1. Signed internal header `x-sanvi-tenant` (HMAC with rotating key) — service-to-service and worker.
2. Session/JWT tenant claim (available from phase 02).
3. Host header → custom domain lookup (cached, populated in phase 08).
4. Host header → `<slug>.platform.sanvi.app` subdomain.
5. Route marked `#[tenant(optional)]` (marketing, platform admin, health) → no context.

Unresolvable host on a tenant-scoped route → `404` (never `403`, to avoid tenant enumeration).
Suspended tenant → `423 Locked` problem+json with a machine-readable `reason`, allow-listed routes
excepted. Resolution results cached in Redis (TTL 60 s) with event-driven invalidation.

### 4. Async backbone (`apps/worker`)

- **Outbox dispatcher**: `SELECT … FOR UPDATE SKIP LOCKED` batches, publishes to Redis Streams
  `sanvi.events.{context}`, marks published, exponential backoff, dead-letter after N attempts,
  metrics for lag.
- **Consumers**: consumer groups per context; handlers idempotent via `platform.processed_events`
  `(consumer, event_id)` unique key; poison messages to DLQ stream with alert.
- **Scheduler**: leader-elected (Redis lock) cron for periodic jobs (purge, retention, sync).
- **Job contract**: every job takes an explicit tenant scope or declares itself platform-scoped.

### 5. API surface (`api/tenancy.yaml`)

| Method | Path | Notes |
|---|---|---|
| POST | `/api/v1/platform/tenants` | Provision (platform-scoped; opened to self-service in 03/04) |
| GET | `/api/v1/platform/tenants` | List with filters, cursor pagination |
| GET | `/api/v1/platform/tenants/{id}` | Detail |
| POST | `/api/v1/platform/tenants/{id}/suspend` \| `/resume` \| `/archive` | Lifecycle |
| GET | `/api/v1/tenant/context` | Current tenant self-description for the frontend bootstrap |
| GET/PUT | `/api/v1/tenant/settings` | Tenant-scoped settings |
| GET | `/api/v1/public/slug-availability?slug=` | Rate-limited, no enumeration signal beyond boolean |

All mutating endpoints accept `Idempotency-Key` and honour it through `platform.idempotency_keys`.

## Work breakdown

1. `tenancy` domain: aggregate, VOs (`TenantSlug`, `DataRegion`), state machine, events, ports.
2. Migrations + `enable_tenant_rls` helper + reserved-slug seed table.
3. SQLx repositories + repository-from-transaction construction pattern.
4. `TxManager` tenant scoping and the compile-time guard; RLS integration tests.
5. Resolution middleware, cache, and negative-path behaviour (404/423).
6. Application use cases: provision, suspend, resume, archive, update settings, read context.
7. HTTP adapters + OpenAPI + problem types; CLI commands `tenant create|list|suspend`.
8. Outbox dispatcher, stream consumers, scheduler, DLQ, metrics.
9. Tenant-aware rate limiting and quota primitives (per-tenant token buckets).
10. Load test: 1k tenants, verify resolution p99 and RLS overhead.

## Testing

- Property test: slug parser accepts/rejects per spec, is idempotent, and rejects reserved words.
- **RLS matrix test** generated from `information_schema`: for every tenant-scoped table, tenant A
  cannot select/update/delete tenant B's rows; a new table without a policy fails the test.
- Middleware tests for all five resolution paths plus hostile inputs (spoofed header, unicode
  homoglyph host, oversized host, `Host` injection).
- Outbox test: crash between commit and publish still delivers exactly once (at-least-once + dedupe).
- Concurrency: two simultaneous provisions of the same slug — one wins, one gets a clean 409.

## Security & compliance

- Tenant enumeration prevented (404s, constant-time-ish slug availability, rate limits).
- Signed internal header with key rotation and clock-skew tolerance; rejected if unsigned.
- Every unscoped (`PlatformScope`) query writes an audit record with reason and actor.
- `region` (`eu`/`jp`/`us`) recorded now so phase 05 can enforce residency and transfer rules without
  a migration. It is the tenant's storage region only — the privacy *jurisdiction* is resolved per
  data subject in phase 05 and is not derivable from it.

## Observability

Metrics: `tenant_resolution_duration`, `tenant_resolution_source_total{source}`,
`outbox_pending_gauge`, `outbox_publish_latency`, `stream_consumer_lag`, `rls_denied_total`.
Alerts on outbox lag > 60 s and any `rls_denied_total` increase (should be zero in production).

## Acceptance criteria

- [ ] A request to `acme.platform.sanvi.app` resolves tenant `acme` and every query in that request
      is automatically restricted to it.
- [ ] Removing the `SET LOCAL` line makes integration tests fail loudly (verified by mutation).
- [ ] Tenant lifecycle transitions emit events that reach a consumer through the outbox.
- [ ] Suspended tenant returns 423 on tenant routes and 200 on the billing allow-list.
- [ ] p99 added latency from resolution + RLS ≤ 5 ms under the 1k-tenant load test.

## Risks

| Risk | Mitigation |
|---|---|
| A developer bypasses `TxManager` and leaks across tenants | Repos are only constructible from a scoped tx; RLS is the second wall; matrix test is the third |
| RLS performance on large tables | `tenant_id` first in composite indexes; `EXPLAIN` assertions in perf tests |
| Redis outage breaks resolution | Cache is an optimisation: fall back to Postgres, degrade with a metric, never fail open to "no tenant" |
| Shared-DB model outgrown by an enterprise tenant | ADR records the extraction path; `region` + `TenantContext` indirection keeps the option open |
