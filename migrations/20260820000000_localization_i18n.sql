-- Phase 06: internationalization & localization.
--
-- Context: localization. Reversible counterpart:
-- 20260820000000_localization_i18n.down.sql
--
-- Invariants:
-- - `supported_locales` is platform reference data (seeded en/ja; a third
--   locale is a data change, never a code change).
-- - `tenant_locales` is tenant-scoped: RLS FORCE via the shared helper.
-- - `translations` holds runtime-editable *overrides only*; the static
--   catalogs ship with the binary and are never read from the database.
--   Platform rows are public to read (they are public copy) and writable
--   only from the audited platform scope; tenant rows obey tenant RLS.

CREATE SCHEMA IF NOT EXISTS localization;

-- ---------------------------------------------------------------------------
-- Platform reference: which locales exist, their display names, order.
-- ---------------------------------------------------------------------------

CREATE TABLE localization.supported_locales (
  code        text PRIMARY KEY,
  name_native text NOT NULL,
  name_en     text NOT NULL,
  enabled     boolean NOT NULL DEFAULT true,
  sort_order  int NOT NULL
);

INSERT INTO localization.supported_locales (code, name_native, name_en, enabled, sort_order)
VALUES
  ('en', 'English', 'English', true, 1),
  ('ja', '日本語', 'Japanese', true, 2);

-- ---------------------------------------------------------------------------
-- Tenant locale settings: default locale, enabled set, IANA timezone.
-- ---------------------------------------------------------------------------

CREATE TABLE localization.tenant_locales (
  tenant_id       bigint        PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  default_locale  text        NOT NULL,
  enabled_locales text[]      NOT NULL,
  timezone        text        NOT NULL DEFAULT 'UTC',
  updated_at      timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT tenant_locales_nonempty CHECK (cardinality(enabled_locales) >= 1)
);

SELECT platform.enable_tenant_rls('localization', 'tenant_locales');

-- ---------------------------------------------------------------------------
-- Runtime-editable copy overrides, merged over the static catalogs.
--
-- `scope` is 'platform' or a tenant bigint (text). Platform rows are readable
-- by the application role without a scope (they are public copy); writes
-- require the audited `sanvi.platform_scope`. Tenant rows are readable and
-- writable only within their tenant's RLS scope.
-- ---------------------------------------------------------------------------

CREATE TABLE localization.translations (
  scope      text        NOT NULL,
  key        text        NOT NULL,
  locale     text        NOT NULL,
  value      text        NOT NULL,
  updated_by bigint,
  updated_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (scope, key, locale),
  CONSTRAINT translations_scope_check
    CHECK (scope = 'platform'
           OR scope ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
  CONSTRAINT translations_locale_check CHECK (locale ~ '^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,4})?$')
);

CREATE INDEX translations_key_idx ON localization.translations (key, locale);

ALTER TABLE localization.translations ENABLE ROW LEVEL SECURITY;
ALTER TABLE localization.translations FORCE ROW LEVEL SECURITY;

-- Reads: platform rows are visible to every role (public copy); tenant rows
-- only within the tenant's RLS scope.
CREATE POLICY translations_read_platform ON localization.translations
  FOR SELECT
  USING (scope = 'platform'
         OR scope = NULLIF(current_setting('sanvi.tenant_id', true), ''));

-- Writes: tenant rows only inside the tenant scope; platform rows only from
-- the audited platform scope.
CREATE POLICY translations_write_tenant ON localization.translations
  FOR INSERT
  WITH CHECK (scope = NULLIF(current_setting('sanvi.tenant_id', true), ''));
CREATE POLICY translations_write_tenant_update ON localization.translations
  FOR UPDATE
  USING (scope = NULLIF(current_setting('sanvi.tenant_id', true), ''))
  WITH CHECK (scope = NULLIF(current_setting('sanvi.tenant_id', true), ''));
CREATE POLICY translations_write_tenant_delete ON localization.translations
  FOR DELETE
  USING (scope = NULLIF(current_setting('sanvi.tenant_id', true), ''));

CREATE POLICY translations_write_platform ON localization.translations
  FOR INSERT
  WITH CHECK (scope = 'platform'
              AND NULLIF(current_setting('sanvi.platform_scope', true), '') = 'on');
CREATE POLICY translations_write_platform_update ON localization.translations
  FOR UPDATE
  USING (scope = 'platform'
         AND NULLIF(current_setting('sanvi.platform_scope', true), '') = 'on')
  WITH CHECK (scope = 'platform'
              AND NULLIF(current_setting('sanvi.platform_scope', true), '') = 'on');

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the NOBYPASSRLS application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA localization TO sanvi_app;

GRANT SELECT ON localization.supported_locales TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON localization.translations TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON localization.tenant_locales TO sanvi_app;
