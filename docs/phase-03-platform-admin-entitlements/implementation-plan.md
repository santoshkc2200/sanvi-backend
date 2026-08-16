# Phase 03 — Platform Admin, RBAC Authoring & Entitlements (backend)

**Target version:** 0.4.0
**Depends on:** phases 01, 02
**Unlocks:** 04 (billing binds plans to entitlements), and feature gating used by 06–10

## Goal

Give the platform operator a real control plane: manage tenants and their lifecycle, author roles and
permissions, define the feature catalog, and gate every optional capability behind an entitlement
that phase 04 will later drive from a paid subscription.

## Scope

**In**
- `platform_admin` context: operator-facing tenant administration, impersonation with guardrails,
  operational search, platform-wide audit views.
- Role & permission **authoring** (custom tenant roles, cloning system roles) on top of phase 02's
  model.
- `Feature` catalog + `Entitlement` resolution + `FeatureGate` used across the codebase.
- Admin-grade audit trail and dangerous-action controls.

**Out**
- Stripe, plans, prices, invoices (phase 04). Entitlements here are set manually by an operator; in
  04 the billing context becomes their source.

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Platform admin identity | Same Kratos identities, `RoleScope::Platform` roles, **MFA required**, separate host (`admin.sanvi.app`) | One identity system, hard privilege boundary |
| Entitlement shape | `(tenant, feature_key) -> Entitlement { enabled, limit, source, expires_at }` | Handles booleans and quotas with one model |
| Entitlement source precedence | `override (operator)` > `subscription` > `plan default` > `feature default` | Support can grant a trial without touching billing |
| Feature flags vs entitlements | Separate: flags are *rollout* (percentage, cohort), entitlements are *commercial* | Conflating them makes pricing changes risky |
| Impersonation | Time-boxed (≤ 30 min), reason required, read-only by default, banner-signalled, fully audited, never for tenants with `privacy_lock` | Support needs it; auditors need proof |
| Custom roles | Tenant-scoped, may only grant permissions the granting actor already holds | Prevents privilege escalation by role authoring |

## Domain model

```rust
// platform_admin
pub struct TenantAdminView { /* read model joining tenancy + identity + billing summary */ }
pub struct ImpersonationGrant { id, actor_id, tenant_id, target_user_id, reason, mode: ReadOnly|ReadWrite,
                                expires_at, revoked_at }

// access (extended)
pub struct FeatureKey(String);            // "advertising.google_ads", "domains.custom", "theming.custom_css"
pub struct Feature { key: FeatureKey, name: String, kind: FeatureKind /*Boolean|Quota*/,
                     default_enabled: bool, default_limit: Option<i64>, visibility: Visibility }
pub struct Entitlement { tenant_id, feature: FeatureKey, enabled: bool, limit: Option<i64>,
                         source: EntitlementSource, expires_at: Option<Timestamp> }
pub trait FeatureGate {
    async fn is_enabled(&self, t: TenantId, f: &FeatureKey) -> Result<bool, AccessError>;
    async fn remaining(&self, t: TenantId, f: &FeatureKey) -> Result<Option<i64>, AccessError>;
    async fn consume(&self, t: TenantId, f: &FeatureKey, n: i64) -> Result<(), QuotaExceeded>;
}
```

Events: `FeatureDefined`, `EntitlementGranted/Revoked/Expired`, `QuotaExceeded`, `RoleCreated`,
`RoleUpdated`, `RoleDeleted`, `ImpersonationStarted/Ended`, `TenantFlagged`.

## Deliverables

### Schema

```sql
CREATE SCHEMA platform_admin;
CREATE TABLE access.features (
  key text PRIMARY KEY, name text NOT NULL, kind text NOT NULL,
  default_enabled boolean NOT NULL DEFAULT false, default_limit bigint,
  visibility text NOT NULL DEFAULT 'public', deprecated_at timestamptz);

CREATE TABLE access.entitlements (
  tenant_id uuid NOT NULL, feature_key text NOT NULL REFERENCES access.features(key),
  enabled boolean NOT NULL, "limit" bigint, source text NOT NULL,
  expires_at timestamptz, granted_by uuid, granted_reason text,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, feature_key, source));

CREATE TABLE access.quota_usage (
  tenant_id uuid, feature_key text, period_start date, used bigint NOT NULL DEFAULT 0,
  PRIMARY KEY (tenant_id, feature_key, period_start));

CREATE TABLE platform_admin.impersonation_grants (...);
CREATE SCHEMA audit;
CREATE TABLE audit.entries (
  id uuid PRIMARY KEY, occurred_at timestamptz NOT NULL DEFAULT now(),
  actor_type text NOT NULL, actor_id uuid, on_behalf_of uuid,
  tenant_id uuid, action text NOT NULL, resource_type text, resource_id text,
  before jsonb, after jsonb, request_id text, ip inet, user_agent text,
  prev_hash bytea, hash bytea NOT NULL);          -- hash chain, tamper-evident
```

`audit.entries` is append-only: no UPDATE/DELETE grant to the app role, enforced by a `BEFORE`
trigger that raises on either.

### Entitlement resolution

Resolution is a pure function over the four sources, memoised per request and cached in Redis
(`sanvi:{tenant}:entitlements`, TTL 300 s) with invalidation on any entitlement/subscription event.
Quota consumption is atomic (`INSERT … ON CONFLICT DO UPDATE … WHERE used + n <= limit RETURNING`),
so two concurrent requests cannot both take the last unit.

`FeatureGate` is injected into every context that has optional capability, and there is a single
`#[feature("domains.custom")]` route attribute so gating is declarative and enumerable.

### API surface (`api/platform-admin.yaml`, `api/access.yaml`, `api/audit.yaml`)

> Note: the operator tenant search is mounted at `/api/v1/platform/admin/tenants`
> (not `/api/v1/platform/tenants`, which the tenancy context already serves with
> its basic cursor list); the two paths coexist without route shadowing.

| Method | Path | Permission |
|---|---|---|
| GET | `/api/v1/platform/admin/tenants` (rich search, filters, sort) | `platform.tenant.read` |
| GET | `/api/v1/platform/admin/tenants/{id}` | `platform.tenant.read` |
| GET | `/api/v1/platform/tenants/{id}/entitlements` | `platform.entitlement.write` |
| POST | `/api/v1/platform/tenants/{id}/entitlements` | `platform.entitlement.write` |
| DELETE | `/api/v1/platform/tenants/{id}/entitlements/{feature}` | `platform.entitlement.write` (+ fresh MFA) |
| GET/POST/PATCH/DELETE | `/api/v1/platform/features` | `platform.feature.write` |
| POST | `/api/v1/platform/impersonations` / `GET` / `DELETE .../{id}` | `platform.impersonate` (+ fresh MFA on create/revoke) |
| GET/POST | `/api/v1/platform/approvals` (`/{id}/approve|reject`) | `platform.approval.read` / `platform.approval.write` (+ fresh MFA on decide) |
| GET | `/api/v1/platform/audit` | `platform.audit.read` |
| GET/POST/PATCH/DELETE | `/api/v1/tenant/roles` | `access.role.*` |
| GET | `/api/v1/tenant/entitlements` | `tenant.entitlement.read` (feature-gated on `access.tenant_entitlements`) |

### Guardrails on dangerous actions

- Fresh-MFA requirement (`aal2` re-auth within 5 min) for impersonation, tenant purge, platform role
  grants, and entitlement overrides above a configured value.
- Optional four-eyes approval: an action can be marked `requires_approval`, creating a pending
  request that a second platform admin approves; both identities recorded.
- All destructive tenant operations are soft, reversible for a grace period, and rate-limited.

## Work breakdown

1. Feature catalog domain + seed of the initial keys used by phases 06–10.
2. Entitlement aggregate, resolver, cache, invalidation on events.
3. Quota counters with atomic consumption and period rollover job.
4. `FeatureGate` port + route attribute + registry test (every declared feature is referenced).
5. Audit context: append-only table, hash chain, writer port, background verifier job.
6. Role authoring use cases with the "cannot grant what you lack" invariant + Keto sync.
7. Impersonation: grant, exchange for a scoped session, banner header, forced expiry, revocation.
8. Platform admin read models (tenant search) — materialised view or query-side projection.
9. Fresh-MFA and four-eyes middleware.
10. OpenAPI, runbooks (`impersonation.md`, `grant-entitlement.md`), operator CLI equivalents.

## Testing

- Entitlement precedence table test (all 4 sources × enabled/limit/expiry combinations).
- Concurrency test for `consume` at the quota boundary (no oversell under 100 parallel calls).
- Escalation tests: tenant admin cannot create a role with `platform.*`; cannot grant a permission
  they lack; cannot edit a system role.
- Impersonation: expires exactly on time, read-only mode blocks writes, every action carries
  `on_behalf_of`, revocation is immediate.
- Audit chain verification test detects a tampered row.

## Security & compliance

- Platform admin surface bound to a separate host and, in production, an IP allow-list/VPN.
- Impersonation into a tenant with an active privacy lock (phase 05) is refused.
- Audit retention ≥ 1 year, exported to cold storage, integrity verified nightly.
- All platform-scoped queries continue to flow through the audited `PlatformScope` from phase 01.

## Observability

Metrics: `entitlement_check_duration`, `feature_gate_denied_total{feature}`,
`quota_exceeded_total{feature}`, `impersonation_active_gauge`, `audit_chain_verify_failures`.
Alert immediately on any audit chain failure or impersonation exceeding the max duration.

## Acceptance criteria

- [x] An operator can grant `advertising.google_ads` to one tenant and the gate flips for that tenant
      only, within 5 s, with an audit entry.
- [x] A tenant admin can create a custom role, assign it, and cannot escalate beyond their own grants.
- [x] Impersonation produces a session that is time-boxed, read-only, banner-flagged, and audited.
- [x] Every optional capability in the codebase is declared in the feature catalog (CI test).
- [x] Audit log is provably append-only and chain-verified.

## Risks

| Risk | Mitigation |
|---|---|
| Entitlement cache staleness gates a paying customer out | Short TTL + event invalidation + fail-open only for `Boolean` features already enabled, never for quotas |
| Impersonation becomes a routine support shortcut | Reason required, dashboards on frequency, monthly review, read-only default |
| Feature-key sprawl | Naming convention `<context>.<capability>`, catalog review in each phase's DoD |
| Audit table growth | Monthly partitioning + archival to object storage after 90 days |
