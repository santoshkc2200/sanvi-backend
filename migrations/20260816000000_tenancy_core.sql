-- Phase 01: tenancy core — schema, RLS machinery and async backbone tables.
--
-- Context: tenancy. Platform helpers: platform.
-- Reversible counterpart: 20260816000000_tenancy_core.down.sql
--
-- Slugs use text + a lowercase unique index (instead of citext) so the
-- migration needs no superuser privileges on any supported PostgreSQL
-- version; the TenantSlug value object enforces lowercase at the boundary
-- and the CHECK keeps hand-written rows honest.

-- Shared RLS helper: one call gives every tenant-scoped table the identical
-- policy (USING + WITH CHECK against the transaction-local
-- `sanvi.tenant_id` GUC). FORCE means even the table owner cannot read
-- without a policy match, so no role can accidentally bypass isolation.
--
-- The NULLIF is load-bearing: once a pooled connection has ever issued
-- `SET LOCAL sanvi.tenant_id`, PostgreSQL registers the custom GUC as a
-- session placeholder whose value reads back as '' (not NULL) after the
-- transaction commits — `''::bigint` would error on every subsequent query.
-- NULLIF collapses the empty placeholder back to NULL ("no tenant"), which
-- is exactly the isolation semantics we want on unscoped connections.
CREATE FUNCTION platform.enable_tenant_rls(schema_name text, table_name text)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
  EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I.%I', schema_name, table_name);
  EXECUTE format('ALTER TABLE %I.%I ENABLE ROW LEVEL SECURITY', schema_name, table_name);
  EXECUTE format('ALTER TABLE %I.%I FORCE ROW LEVEL SECURITY', schema_name, table_name);
  EXECUTE format($policy$
    CREATE POLICY tenant_isolation ON %1$I.%2$I
      USING (tenant_id = NULLIF(current_setting('sanvi.tenant_id', true), '')::bigint)
      WITH CHECK (tenant_id = NULLIF(current_setting('sanvi.tenant_id', true), '')::bigint)
  $policy$, schema_name, table_name);
END;
$$;

CREATE SCHEMA IF NOT EXISTS tenancy;

-- Tenants are platform-scoped: no tenant_id column, no RLS. Access happens
-- exclusively through PlatformScope (audited) — see crates/platform/persistence.
CREATE TABLE tenancy.tenants (
  id                 bigint      PRIMARY KEY,
  slug               text        NOT NULL,
  display_name       text        NOT NULL,
  status             text        NOT NULL,
  region             text        NOT NULL,
  default_locale     text        NOT NULL DEFAULT 'en',
  suspended_reason   text,
  created_at         timestamptz NOT NULL DEFAULT now(),
  updated_at         timestamptz NOT NULL DEFAULT now(),
  archived_at        timestamptz,
  purge_requested_at timestamptz
);

-- Slugs are always stored lowercase (enforced by the TenantSlug value
-- object); the check keeps hand-written rows honest too, and the unique
-- index on lower(slug) gives citext-equivalent uniqueness.
ALTER TABLE tenancy.tenants
  ADD CONSTRAINT tenants_slug_lowercase CHECK (slug = lower(slug));

CREATE UNIQUE INDEX tenants_slug_unique ON tenancy.tenants (lower(slug));

-- The first tenant-scoped table: RLS FORCE + policy via the shared helper.
CREATE TABLE tenancy.tenant_settings (
  tenant_id  bigint      NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  key        text        NOT NULL,
  value      jsonb       NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, key)
);

SELECT platform.enable_tenant_rls('tenancy', 'tenant_settings');

-- Slug reservations: taken at provision time before the tenant row exists, so
-- two concurrent provisions of the same slug can never both succeed (one gets
-- a clean conflict on the unique constraint).
CREATE TABLE tenancy.slug_reservations (
  slug        text        NOT NULL PRIMARY KEY,
  reserved_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT slug_reservations_lowercase CHECK (slug = lower(slug))
);

-- Operator-extensible reserved-word blocklist (complements the compile-time
-- blocklist in the TenantSlug value object).
CREATE TABLE tenancy.reserved_slugs (
  slug       text        PRIMARY KEY,
  reason     text        NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT reserved_slugs_lowercase CHECK (slug = lower(slug))
);

INSERT INTO tenancy.reserved_slugs (slug, reason) VALUES
  ('www', 'reserved: impersonation/phishing surface'),
  ('api', 'reserved: platform infrastructure'),
  ('admin', 'reserved: impersonation/phishing surface'),
  ('mail', 'reserved: DNS/email infrastructure'),
  ('mailer', 'reserved: DNS/email infrastructure'),
  ('smtp', 'reserved: DNS/email infrastructure'),
  ('app', 'reserved: platform infrastructure'),
  ('apps', 'reserved: platform infrastructure'),
  ('platform', 'reserved: platform itself'),
  ('sanvi', 'reserved: platform itself'),
  ('root', 'reserved: impersonation/phishing surface'),
  ('sys', 'reserved: impersonation/phishing surface'),
  ('system', 'reserved: impersonation/phishing surface'),
  ('billing', 'reserved: platform infrastructure'),
  ('payments', 'reserved: platform infrastructure'),
  ('auth', 'reserved: impersonation/phishing surface'),
  ('login', 'reserved: impersonation/phishing surface'),
  ('signup', 'reserved: impersonation/phishing surface'),
  ('register', 'reserved: impersonation/phishing surface'),
  ('webhook', 'reserved: platform infrastructure'),
  ('webhooks', 'reserved: platform infrastructure'),
  ('checkout', 'reserved: platform infrastructure'),
  ('account', 'reserved: impersonation/phishing surface'),
  ('accounts', 'reserved: impersonation/phishing surface'),
  ('status', 'reserved: platform infrastructure'),
  ('support', 'reserved: impersonation/phishing surface'),
  ('help', 'reserved: impersonation/phishing surface'),
  ('docs', 'reserved: impersonation/phishing surface'),
  ('blog', 'reserved: impersonation/phishing surface'),
  ('shop', 'reserved: impersonation/phishing surface'),
  ('store', 'reserved: impersonation/phishing surface'),
  ('test', 'reserved: development hygiene'),
  ('testing', 'reserved: development hygiene'),
  ('demo', 'reserved: development hygiene'),
  ('sandbox', 'reserved: development hygiene'),
  ('staging', 'reserved: development hygiene'),
  ('stage', 'reserved: development hygiene'),
  ('prod', 'reserved: development hygiene'),
  ('production', 'reserved: development hygiene'),
  ('dev', 'reserved: development hygiene'),
  ('development', 'reserved: development hygiene'),
  ('dashboard', 'reserved: impersonation/phishing surface'),
  ('manage', 'reserved: impersonation/phishing surface'),
  ('portal', 'reserved: impersonation/phishing surface'),
  ('cdn', 'reserved: platform infrastructure'),
  ('static', 'reserved: platform infrastructure'),
  ('assets', 'reserved: platform infrastructure'),
  ('metrics', 'reserved: platform infrastructure'),
  ('graphql', 'reserved: platform infrastructure');

-- Idempotency keys are identified by (key, scope), not `key` alone: `scope`
-- names the operation an Idempotency-Key was issued for (and, once
-- tenant-scoped mutating endpoints wire this store up, will carry the
-- tenant id too). Without scope in the identity, two callers that happen to
-- reuse the same client-supplied Idempotency-Key value for different
-- operations — or, worse, different tenants — would collide and one would
-- be served the other's stored response.
ALTER TABLE platform.idempotency_keys DROP CONSTRAINT idempotency_keys_pkey;
ALTER TABLE platform.idempotency_keys ADD PRIMARY KEY (key, scope);

-- Async backbone: consumer deduplication, outbox dead-letter, platform audit.
CREATE TABLE platform.processed_events (
  consumer     text        NOT NULL,
  event_id     bigint      NOT NULL,
  processed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (consumer, event_id)
);

CREATE TABLE platform.outbox_dlq (
  LIKE platform.outbox INCLUDING ALL,
  dead_lettered_at timestamptz NOT NULL DEFAULT now(),
  failure          text        NOT NULL
);

-- Every unscoped (PlatformScope) transaction writes an audit row with its
-- reason and actor before any statement runs.
CREATE TABLE platform.audit_log (
  id     bigint      PRIMARY KEY,
  actor  text        NOT NULL,
  action text        NOT NULL,
  reason text,
  detail jsonb       NOT NULL DEFAULT '{}',
  at     timestamptz NOT NULL DEFAULT now()
);

-- Least-privilege grants: the NOBYPASSRLS application role.
GRANT USAGE ON SCHEMA tenancy TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON tenancy.tenants TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON tenancy.tenant_settings TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON tenancy.slug_reservations TO sanvi_app;
GRANT SELECT ON tenancy.reserved_slugs TO sanvi_app;

-- The dispatcher deletes rows it moves to the DLQ; the scheduler sweeps
-- expired idempotency keys; consumers claim processed events; PlatformScope
-- writes audit rows.
GRANT DELETE ON platform.outbox TO sanvi_app;
GRANT INSERT ON platform.outbox_dlq TO sanvi_app;
GRANT SELECT, INSERT ON platform.processed_events TO sanvi_app;
GRANT DELETE ON platform.idempotency_keys TO sanvi_app;
GRANT INSERT ON platform.audit_log TO sanvi_app;
