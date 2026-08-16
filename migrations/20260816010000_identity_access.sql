-- Phase 02: identity & access — local projections, memberships, invitations,
-- account linking, and the role/permission catalog.
--
-- Contexts: identity, access. Reversible counterpart:
-- 20260816010000_identity_access.down.sql
--
-- Emails use text + a lowercase unique index (instead of citext) for the same
-- reason as tenancy slugs: no extension/privilege requirements on any
-- supported PostgreSQL version; the Email value object enforces lowercase at
-- the boundary and the CHECK keeps hand-written rows honest.

-- Shared RLS helper v2: identical policy for every tenant-scoped table, with
-- one audited escape hatch — `sanvi.platform_scope` is set only by
-- `TxManager::begin_unscoped` (PlatformScope, which writes an audit row first)
-- so cross-tenant maintenance/reconciliation can read tenant rows on the
-- record. The NULLIF caveat from phase 01 applies unchanged: a committed
-- GUC placeholder reads back as '', never NULL.
CREATE OR REPLACE FUNCTION platform.enable_tenant_rls(schema_name text, table_name text)
RETURNS void
LANGUAGE plpgsql
AS $$
BEGIN
  EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I.%I', schema_name, table_name);
  EXECUTE format('ALTER TABLE %I.%I ENABLE ROW LEVEL SECURITY', schema_name, table_name);
  EXECUTE format('ALTER TABLE %I.%I FORCE ROW LEVEL SECURITY', schema_name, table_name);
  EXECUTE format($policy$
    CREATE POLICY tenant_isolation ON %1$I.%2$I
      USING (
        current_setting('sanvi.platform_scope', true) = 'on'
        OR tenant_id = NULLIF(current_setting('sanvi.tenant_id', true), '')::uuid
      )
      WITH CHECK (
        current_setting('sanvi.platform_scope', true) = 'on'
        OR tenant_id = NULLIF(current_setting('sanvi.tenant_id', true), '')::uuid
      )
  $policy$, schema_name, table_name);
END;
$$;

-- Phase-01 tenant_settings gets the v2 policy too, so every tenant-scoped
-- table in the database behaves identically.
SELECT platform.enable_tenant_rls('tenancy', 'tenant_settings');

CREATE SCHEMA IF NOT EXISTS identity;
CREATE SCHEMA IF NOT EXISTS access;

-- ---------------------------------------------------------------------------
-- access: the role catalog and role→permission assignments.
-- ---------------------------------------------------------------------------

CREATE TABLE access.roles (
  id         uuid        PRIMARY KEY,
  scope      text        NOT NULL,
  tenant_id  uuid,
  key        text        NOT NULL,
  name       text        NOT NULL,
  is_system  boolean     NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT roles_scope_check CHECK (scope IN ('platform', 'tenant')),
  -- Platform roles are never tenant-bound. Tenant-scoped custom roles carry
  -- a tenant_id; system tenant roles are templates with tenant_id NULL.
  CONSTRAINT roles_binding_check CHECK (
    (scope = 'platform' AND tenant_id IS NULL)
    OR scope = 'tenant'
  ),
  CONSTRAINT roles_key_lowercase CHECK (key = lower(key))
);

-- Postgres UNIQUE treats NULLs as distinct, so system templates (tenant_id
-- NULL) and per-tenant custom roles need separate uniqueness guarantees.
CREATE UNIQUE INDEX roles_system_key_unique
  ON access.roles (scope, key) WHERE tenant_id IS NULL;
CREATE UNIQUE INDEX roles_tenant_key_unique
  ON access.roles (scope, tenant_id, key) WHERE tenant_id IS NOT NULL;

-- RLS: tenant scope sees only that tenant's custom roles; the audited
-- PlatformScope (sanvi.platform_scope) sees everything — the authorizer and
-- the seed jobs run there.
SELECT platform.enable_tenant_rls('access', 'roles');

CREATE TABLE access.role_permissions (
  role_id    uuid NOT NULL REFERENCES access.roles(id) ON DELETE CASCADE,
  permission text NOT NULL,
  PRIMARY KEY (role_id, permission)
);

-- ---------------------------------------------------------------------------
-- identity: local projection of Kratos identities, memberships, invitations,
-- linked accounts and single-use linking intents.
-- ---------------------------------------------------------------------------

CREATE TABLE identity.users (
  id                 uuid        PRIMARY KEY,
  kratos_identity_id uuid        NOT NULL UNIQUE,
  primary_email      text        NOT NULL,
  email_verified     boolean     NOT NULL DEFAULT false,
  status             text        NOT NULL DEFAULT 'active',
  locale             text,
  created_at         timestamptz NOT NULL DEFAULT now(),
  deactivated_at     timestamptz,
  CONSTRAINT users_email_lowercase CHECK (primary_email = lower(primary_email)),
  CONSTRAINT users_status_check CHECK (status IN ('active', 'deactivated'))
);

-- Partial index: a deactivated user releases their email for re-registration,
-- exactly one active user may hold an email at a time.
CREATE UNIQUE INDEX users_primary_email_unique
  ON identity.users (primary_email) WHERE deactivated_at IS NULL;

CREATE TABLE identity.memberships (
  tenant_id  uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  user_id    uuid        NOT NULL REFERENCES identity.users(id) ON DELETE CASCADE,
  status     text        NOT NULL DEFAULT 'active',
  invited_by uuid,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, user_id),
  CONSTRAINT memberships_status_check CHECK (status IN ('active', 'suspended'))
);

SELECT platform.enable_tenant_rls('identity', 'memberships');

CREATE TABLE identity.membership_roles (
  tenant_id uuid NOT NULL,
  user_id   uuid NOT NULL,
  role_id   uuid NOT NULL REFERENCES access.roles(id) ON DELETE CASCADE,
  PRIMARY KEY (tenant_id, user_id, role_id),
  FOREIGN KEY (tenant_id, user_id)
    REFERENCES identity.memberships(tenant_id, user_id) ON DELETE CASCADE
);

SELECT platform.enable_tenant_rls('identity', 'membership_roles');

CREATE TABLE identity.linked_accounts (
  user_id    uuid        NOT NULL REFERENCES identity.users(id) ON DELETE CASCADE,
  provider   text        NOT NULL,
  subject    text        NOT NULL,
  linked_via text        NOT NULL,
  linked_at  timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (provider, subject),
  CONSTRAINT linked_accounts_via_check
    CHECK (linked_via IN ('registration', 'auto_verified', 'challenge', 'settings_flow'))
);

CREATE TABLE identity.invitations (
  id          uuid        PRIMARY KEY,
  tenant_id   uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  email       text        NOT NULL,
  role_ids    uuid[]      NOT NULL,
  token_hash  text        NOT NULL UNIQUE,
  invited_by  uuid,
  created_at  timestamptz NOT NULL DEFAULT now(),
  expires_at  timestamptz NOT NULL,
  accepted_at timestamptz,
  CONSTRAINT invitations_email_lowercase CHECK (email = lower(email)),
  CONSTRAINT invitations_roles_nonempty CHECK (cardinality(role_ids) > 0)
);

SELECT platform.enable_tenant_rls('identity', 'invitations');

-- Linking intents: the account-linking challenge's single-use, short-lived,
-- nonce-bound state. Platform-scoped (created before the user is signed in).
CREATE TABLE identity.link_intents (
  nonce_hash text        PRIMARY KEY,
  payload    jsonb       NOT NULL,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the NOBYPASSRLS application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA identity TO sanvi_app;
GRANT USAGE ON SCHEMA access TO sanvi_app;

GRANT SELECT, INSERT, UPDATE ON identity.users TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON identity.memberships TO sanvi_app;
GRANT SELECT, INSERT, DELETE ON identity.membership_roles TO sanvi_app;
GRANT SELECT, INSERT, DELETE ON identity.linked_accounts TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON identity.invitations TO sanvi_app;
GRANT SELECT, INSERT, DELETE ON identity.link_intents TO sanvi_app;

GRANT SELECT, INSERT, UPDATE, DELETE ON access.roles TO sanvi_app;
GRANT SELECT, INSERT, DELETE ON access.role_permissions TO sanvi_app;
