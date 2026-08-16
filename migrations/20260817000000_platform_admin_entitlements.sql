-- Phase 03: platform admin, RBAC authoring & entitlements.
--
-- Contexts: access (features, entitlements, quota usage), platform_admin
-- (impersonation grants, four-eyes pending approvals), audit (append-only,
-- hash-chained business audit trail). Reversible counterpart:
-- 20260817000000_platform_admin_entitlements.down.sql
--
-- Entitlement resolution reads `access.entitlements` (override and, from
-- phase 04 onward, subscription rows) and composes them with
-- `access.features` defaults; `access.quota_usage` backs atomic quota
-- consumption. `audit.entries` is append-only at two levels: the app role
-- has no UPDATE/DELETE grants, and a BEFORE trigger raises on either so no
-- elevated session can quietly rewrite history. Each row carries a hash
-- over the previous row (a single global chain), so tampering is detectable
-- by recomputation rather than trust.

CREATE SCHEMA IF NOT EXISTS platform_admin;
CREATE SCHEMA IF NOT EXISTS audit;

-- ---------------------------------------------------------------------------
-- access: the feature catalog.
-- ---------------------------------------------------------------------------

CREATE TABLE access.features (
  key             text        PRIMARY KEY,
  name            text        NOT NULL,
  kind            text        NOT NULL,
  default_enabled boolean     NOT NULL DEFAULT false,
  default_limit   bigint,
  visibility      text        NOT NULL DEFAULT 'public',
  deprecated_at   timestamptz,
  CONSTRAINT features_kind_check CHECK (kind IN ('boolean', 'quota')),
  CONSTRAINT features_visibility_check CHECK (visibility IN ('public', 'hidden')),
  CONSTRAINT features_key_lowercase CHECK (key = lower(key)),
  CONSTRAINT features_default_limit_check CHECK (
    (kind = 'boolean' AND default_limit IS NULL)
    OR kind = 'quota'
  )
);

-- ---------------------------------------------------------------------------
-- access: per-tenant entitlements. Rows exist only for explicit grants
-- (`override` today, `subscription` from phase 04); plan/feature defaults
-- are synthetic and composed at resolution time.
-- ---------------------------------------------------------------------------

CREATE TABLE access.entitlements (
  tenant_id      uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  feature_key    text        NOT NULL REFERENCES access.features(key),
  enabled        boolean     NOT NULL,
  "limit"        bigint,
  source         text        NOT NULL,
  expires_at     timestamptz,
  granted_by     uuid,
  granted_reason text,
  updated_at     timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, feature_key, source),
  CONSTRAINT entitlements_source_check
    CHECK (source IN ('override', 'subscription', 'plan_default', 'feature_default')),
  CONSTRAINT entitlements_limit_check CHECK (
    "limit" IS NULL OR "limit" >= 0
  )
);

SELECT platform.enable_tenant_rls('access', 'entitlements');

-- ---------------------------------------------------------------------------
-- access: quota counters. Consumption is a single atomic statement
-- (INSERT ... ON CONFLICT ... WHERE used + n <= limit RETURNING), so two
-- concurrent requests cannot both take the last unit. The period key is the
-- first day of the usage month; a worker job rolls old periods over.
-- ---------------------------------------------------------------------------

CREATE TABLE access.quota_usage (
  tenant_id    uuid   NOT NULL,
  feature_key  text   NOT NULL REFERENCES access.features(key),
  period_start date   NOT NULL,
  used         bigint NOT NULL DEFAULT 0,
  PRIMARY KEY (tenant_id, feature_key, period_start),
  CONSTRAINT quota_usage_used_nonnegative CHECK (used >= 0)
);

SELECT platform.enable_tenant_rls('access', 'quota_usage');

-- ---------------------------------------------------------------------------
-- platform_admin: impersonation grants. Platform-scoped in practice (tenant
-- users have no endpoint that reads them), but RLS keeps the invariant
-- uniform for any tenant_id-carrying table.
-- ---------------------------------------------------------------------------

CREATE TABLE platform_admin.impersonation_grants (
  id             uuid        PRIMARY KEY,
  actor_id       uuid        NOT NULL,
  tenant_id      uuid        NOT NULL,
  target_user_id uuid        NOT NULL,
  reason         text        NOT NULL,
  mode           text        NOT NULL,
  expires_at     timestamptz NOT NULL,
  created_at     timestamptz NOT NULL DEFAULT now(),
  revoked_at     timestamptz,
  revoked_by     uuid,
  CONSTRAINT impersonation_mode_check CHECK (mode IN ('read_only', 'read_write')),
  CONSTRAINT impersonation_reason_nonempty CHECK (length(btrim(reason)) > 0),
  CONSTRAINT impersonation_expiry_check CHECK (expires_at > created_at)
);

CREATE INDEX impersonation_grants_active_idx
  ON platform_admin.impersonation_grants (tenant_id, expires_at)
  WHERE revoked_at IS NULL;

SELECT platform.enable_tenant_rls('platform_admin', 'impersonation_grants');

-- ---------------------------------------------------------------------------
-- platform_admin: four-eyes pending approvals. A dangerous action (e.g. an
-- entitlement override above the configured threshold) can be written as a
-- pending request; a *different* platform admin approves it, and only then
-- is the action executed. Both identities are recorded.
-- ---------------------------------------------------------------------------

CREATE TABLE platform_admin.pending_approvals (
  id           uuid        PRIMARY KEY,
  requested_by uuid        NOT NULL,
  action_type  text        NOT NULL,
  payload      jsonb       NOT NULL,
  status       text        NOT NULL DEFAULT 'pending',
  requested_at timestamptz NOT NULL DEFAULT now(),
  decided_by   uuid,
  decided_at   timestamptz,
  CONSTRAINT pending_approvals_status_check
    CHECK (status IN ('pending', 'approved', 'rejected', 'executed')),
  CONSTRAINT pending_approvals_action_nonempty CHECK (length(btrim(action_type)) > 0),
  -- A decision is only meaningful on a request that is still pending.
  CONSTRAINT pending_approvals_decider_check CHECK (
    status = 'pending' OR decided_by IS NOT NULL
  )
);

CREATE INDEX pending_approvals_pending_idx
  ON platform_admin.pending_approvals (requested_at)
  WHERE status = 'pending';

-- ---------------------------------------------------------------------------
-- audit: the business audit trail — append-only, hash-chained.
--
-- Retention: rows are never deletable by the application role, which
-- guarantees the >= 1 year retention by construction; archival to cold
-- storage happens under the migrator role as a documented maintenance
-- procedure (see docs/runbooks/audit-chain.md). Integrity is recomputed by
-- a nightly verifier job; any mismatch alerts immediately.
-- ---------------------------------------------------------------------------

CREATE TABLE audit.entries (
  seq           bigserial   NOT NULL,
  id            uuid        PRIMARY KEY,
  occurred_at   timestamptz NOT NULL DEFAULT now(),
  actor_type    text        NOT NULL,
  actor_id      uuid,
  on_behalf_of  uuid,
  tenant_id     uuid,
  action        text        NOT NULL,
  resource_type text,
  resource_id   text,
  before        jsonb,
  after         jsonb,
  request_id    text,
  ip            inet,
  user_agent    text,
  prev_hash     bytea       NOT NULL,
  hash          bytea       NOT NULL,
  CONSTRAINT audit_actor_type_check
    CHECK (actor_type IN ('user', 'platform_admin', 'system')),
  CONSTRAINT audit_action_nonempty CHECK (length(btrim(action)) > 0)
);

-- `seq` is the chain's ordering authority: appends are serialized with an
-- advisory lock and link to the row with the highest seq, so ordering is
-- insertion order even when two rows share the same `occurred_at`. The
-- value is not covered by the row hash (it is assigned by the database on
-- INSERT, after the hash is computed) — it is protected by the append-only
-- grants and trigger instead.
CREATE UNIQUE INDEX audit_entries_seq_unique ON audit.entries (seq);

CREATE INDEX audit_entries_occurred_at_idx ON audit.entries (occurred_at DESC);
CREATE INDEX audit_entries_tenant_idx ON audit.entries (tenant_id, occurred_at DESC);
CREATE INDEX audit_entries_actor_idx ON audit.entries (actor_id, occurred_at DESC);
CREATE INDEX audit_entries_action_idx ON audit.entries (action, occurred_at DESC);

-- RLS for the same reason as every other tenant_id-carrying table: a
-- tenant scoped transaction can never touch another tenant's audit rows.
-- All audit reads/writes run under the audited PlatformScope, which passes
-- the policy's escape hatch.
SELECT platform.enable_tenant_rls('audit', 'entries');

-- Append-only enforcement: even a session that somehow holds UPDATE/DELETE
-- privileges (e.g. the migrator role) cannot modify history.
CREATE FUNCTION audit.entries_append_only()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
  RAISE EXCEPTION 'audit.entries is append-only (no UPDATE/DELETE)'
    USING ERRCODE = 'insufficient_privilege';
END;
$$;

CREATE TRIGGER entries_no_update
  BEFORE UPDATE OR DELETE ON audit.entries
  FOR EACH ROW
  EXECUTE FUNCTION audit.entries_append_only();

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the NOBYPASSRLS application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA platform_admin TO sanvi_app;
GRANT USAGE ON SCHEMA audit TO sanvi_app;

GRANT SELECT, INSERT, UPDATE ON access.features TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON access.entitlements TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON access.quota_usage TO sanvi_app;

GRANT SELECT, INSERT, UPDATE ON platform_admin.impersonation_grants TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON platform_admin.pending_approvals TO sanvi_app;

-- Append-only: INSERT and SELECT only; the trigger is the second lock.
GRANT SELECT, INSERT ON audit.entries TO sanvi_app;
-- The `seq` bigserial's sequence (INSERT needs USAGE on it).
GRANT USAGE, SELECT ON SEQUENCE audit.entries_seq_seq TO sanvi_app;
