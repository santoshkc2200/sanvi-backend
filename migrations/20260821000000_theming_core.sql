-- Phase 07: theming & pluggable templates.
--
-- Context: theming. Reversible counterpart:
-- 20260821000000_theming_core.down.sql
--
-- Invariants:
-- - `themes` / `theme_versions` are platform reference data: a theme is an
--   installable, versioned artifact. Rows in `theme_versions` are immutable
--   after insert (trigger-enforced); withdrawal is `yanked_at`, never
--   deletion, so a pinned tenant keeps rendering.
-- - `tenant_themes` holds one row per tenant per state (`draft` | `live`),
--   tenant-scoped via the shared RLS helper. Publishing copies draft → live
--   and appends an immutable snapshot to `tenant_theme_revisions` in the
--   same transaction; rollback is a pointer move over the snapshots.
-- - The `base` theme is seeded as platform reference data (like
--   `localization.supported_locales`); new themes are installed by
--   publishing artifacts (`apps/cli theme publish`), never by migration.

CREATE SCHEMA IF NOT EXISTS theming;

-- ---------------------------------------------------------------------------
-- Platform registry: which themes exist and who publishes them.
-- ---------------------------------------------------------------------------

CREATE TABLE theming.themes (
  id               bigint        PRIMARY KEY,
  key              text        UNIQUE NOT NULL,
  publisher        text        NOT NULL,
  visibility       text        NOT NULL,
  required_feature text,
  status           text        NOT NULL DEFAULT 'active',
  created_at       timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT themes_visibility_check CHECK (visibility IN ('public', 'premium')),
  CONSTRAINT themes_status_check CHECK (status IN ('active', 'deprecated'))
);

-- ---------------------------------------------------------------------------
-- Immutable theme versions. `manifest` is the validated theme.json; `tokens`
-- holds the merged DTCG token document per mode (`{light: {...}, dark: {...}}`)
-- so resolution never depends on object storage at request time. The artifact
-- (tokens + assets zip) is content-addressed and digest-pinned.
-- ---------------------------------------------------------------------------

CREATE TABLE theming.theme_versions (
  id               bigint        PRIMARY KEY,
  theme_id         bigint        NOT NULL REFERENCES theming.themes(id),
  semver           text        NOT NULL,
  theme_api_range  text        NOT NULL,
  manifest         jsonb       NOT NULL,
  tokens           jsonb       NOT NULL,
  artifact_uri     text        NOT NULL,
  artifact_digest  text        NOT NULL,
  published_at     timestamptz NOT NULL DEFAULT now(),
  yanked_at        timestamptz,
  UNIQUE (theme_id, semver),
  CONSTRAINT theme_versions_semver_check CHECK (semver ~ '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$')
);

CREATE FUNCTION theming.enforce_version_immutability() RETURNS trigger AS $$
BEGIN
  IF TG_OP = 'DELETE' THEN
    RAISE EXCEPTION 'theme versions are immutable; withdraw with yanked_at';
  END IF;
  IF TG_OP = 'UPDATE' THEN
    IF OLD.id <> NEW.id
       OR OLD.theme_id <> NEW.theme_id
       OR OLD.semver <> NEW.semver
       OR OLD.theme_api_range <> NEW.theme_api_range
       OR OLD.manifest <> NEW.manifest
       OR OLD.tokens <> NEW.tokens
       OR OLD.artifact_uri <> NEW.artifact_uri
       OR OLD.artifact_digest <> NEW.artifact_digest
       OR OLD.published_at <> NEW.published_at THEN
      RAISE EXCEPTION 'theme versions are immutable; withdraw with yanked_at';
    END IF;
    IF OLD.yanked_at IS NOT NULL AND NEW.yanked_at IS DISTINCT FROM OLD.yanked_at THEN
      RAISE EXCEPTION 'a yanked version cannot be un-yanked';
    END IF;
    RETURN NEW;
  END IF;
  RETURN NEW;
END
$$ LANGUAGE plpgsql;

CREATE TRIGGER theme_versions_immutable
  BEFORE UPDATE OR DELETE ON theming.theme_versions
  FOR EACH ROW EXECUTE FUNCTION theming.enforce_version_immutability();

-- ---------------------------------------------------------------------------
-- Tenant theme state: one row per (tenant, state). RLS FORCE.
-- ---------------------------------------------------------------------------

CREATE TABLE theming.tenant_themes (
  tenant_id        bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  state            text        NOT NULL,
  theme_id         bigint        NOT NULL REFERENCES theming.themes(id),
  semver           text        NOT NULL,
  token_overrides  jsonb       NOT NULL DEFAULT '{}',
  layout_overrides jsonb       NOT NULL DEFAULT '{}',
  assets           jsonb       NOT NULL DEFAULT '{}',
  custom_css       text,
  revision         int         NOT NULL DEFAULT 0,
  updated_by       bigint,
  updated_at       timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, state),
  CONSTRAINT tenant_themes_state_check CHECK (state IN ('draft', 'live'))
);

SELECT platform.enable_tenant_rls('theming', 'tenant_themes');

-- ---------------------------------------------------------------------------
-- Immutable publish snapshots: rollback restores the exact previous look,
-- including assets and custom CSS.
-- ---------------------------------------------------------------------------

CREATE TABLE theming.tenant_theme_revisions (
  tenant_id    bigint        NOT NULL,
  revision     int         NOT NULL,
  snapshot     jsonb       NOT NULL,
  published_at timestamptz NOT NULL,
  published_by bigint,
  PRIMARY KEY (tenant_id, revision),
  CONSTRAINT tenant_theme_revisions_tenant_fk
    FOREIGN KEY (tenant_id) REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  CONSTRAINT tenant_theme_revisions_revision_check CHECK (revision >= 1)
);

SELECT platform.enable_tenant_rls('theming', 'tenant_theme_revisions');

-- ---------------------------------------------------------------------------
-- Platform reference seed: the base theme, version 1.0.0. First-party themes
-- are otherwise installed via `apps/cli theme publish` (artifact pipeline),
-- exactly like a third-party theme would be.
-- ---------------------------------------------------------------------------

INSERT INTO theming.themes (id, key, publisher, visibility, required_feature, status)
VALUES ('10', 'base', 'sanvi', 'public', NULL, 'active');

INSERT INTO theming.theme_versions
  (id, theme_id, semver, theme_api_range, manifest, tokens, artifact_uri, artifact_digest)
VALUES (
  '11',
  '10',
  '1.0.0',
  '^1.0.0',
  '{
    "id": "base",
    "version": "1.0.0",
    "themeApi": "^1.0.0",
    "name": { "en": "Base", "ja": "ベース" },
    "capabilities": ["dark-mode", "rtl-ready", "cjk-typography"],
    "tokens": { "light": "tokens/light.json", "dark": "tokens/dark.json" },
    "overridable": ["color.brand.*", "color.background.*", "color.text.*", "color.border.*", "font.family.*", "font.size.*", "radius.*", "spacing.scale"],
    "layouts": {
      "storefront.home": { "slots": ["hero", "featured", "testimonials", "cta"] },
      "storefront.product": { "slots": ["gallery", "details", "related"] },
      "storefront.checkout": { "slots": ["summary", "payment"], "locked": true }
    },
    "assets": { "preview": "preview.png" },
    "fonts": [
      { "family": "Inter", "subsets": ["latin"], "source": "self-hosted" },
      { "family": "Noto Sans JP", "subsets": ["latin", "japanese"], "source": "self-hosted" }
    ]
  }',
  '{
    "light": {
      "color": {
        "background": {
          "primary": { "$value": "#ffffff", "$type": "color" },
          "secondary": { "$value": "#f6f7f9", "$type": "color" },
          "inverse": { "$value": "#1a1d23", "$type": "color" }
        },
        "text": {
          "primary": { "$value": "#1a1d23", "$type": "color" },
          "secondary": { "$value": "#4a505c", "$type": "color" },
          "inverse": { "$value": "#ffffff", "$type": "color" }
        },
        "border": { "primary": { "$value": "#d7dbe2", "$type": "color" } },
        "brand": {
          "primary": { "$value": "#2563eb", "$type": "color" },
          "hover": { "$value": "#1d4ed8", "$type": "color" },
          "contrast": { "$value": "#ffffff", "$type": "color" }
        },
        "focus": { "ring": { "$value": "#1d4ed8", "$type": "color" } },
        "status": {
          "success": { "$value": "#15803d", "$type": "color" },
          "warning": { "$value": "#b45309", "$type": "color" },
          "danger": { "$value": "#b91c1c", "$type": "color" },
          "info": { "$value": "#1d4ed8", "$type": "color" }
        }
      },
      "font": {
        "family": {
          "base": { "$value": "Inter, system-ui, sans-serif", "$type": "fontFamily" },
          "heading": { "$value": "Inter, system-ui, sans-serif", "$type": "fontFamily" },
          "mono": { "$value": "ui-monospace, SFMono-Regular, Menlo, monospace", "$type": "fontFamily" }
        },
        "size": {
          "sm": { "$value": "0.875rem", "$type": "dimension" },
          "base": { "$value": "1rem", "$type": "dimension" },
          "lg": { "$value": "1.125rem", "$type": "dimension" },
          "xl": { "$value": "1.375rem", "$type": "dimension" }
        },
        "weight": {
          "normal": { "$value": 400, "$type": "fontWeight" },
          "bold": { "$value": 700, "$type": "fontWeight" }
        }
      },
      "radius": {
        "sm": { "$value": "0.25rem", "$type": "dimension" },
        "md": { "$value": "0.5rem", "$type": "dimension" },
        "lg": { "$value": "0.75rem", "$type": "dimension" },
        "full": { "$value": "9999px", "$type": "dimension" }
      },
      "spacing": {
        "scale": { "$value": 1, "$type": "number" },
        "unit": { "$value": "0.25rem", "$type": "dimension" }
      }
    },
    "dark": {
      "color": {
        "background": {
          "primary": { "$value": "#0f1117", "$type": "color" },
          "secondary": { "$value": "#171a21", "$type": "color" },
          "inverse": { "$value": "#ffffff", "$type": "color" }
        },
        "text": {
          "primary": { "$value": "#e6e8ee", "$type": "color" },
          "secondary": { "$value": "#9aa1ad", "$type": "color" },
          "inverse": { "$value": "#0f1117", "$type": "color" }
        },
        "border": { "primary": { "$value": "#2a2f3a", "$type": "color" } },
        "brand": {
          "primary": { "$value": "#2563eb", "$type": "color" },
          "hover": { "$value": "#1d4ed8", "$type": "color" },
          "contrast": { "$value": "#ffffff", "$type": "color" }
        },
        "focus": { "ring": { "$value": "#3b82f6", "$type": "color" } },
        "status": {
          "success": { "$value": "#4ade80", "$type": "color" },
          "warning": { "$value": "#fbbf24", "$type": "color" },
          "danger": { "$value": "#f87171", "$type": "color" },
          "info": { "$value": "#60a5fa", "$type": "color" }
        }
      },
      "font": {
        "family": {
          "base": { "$value": "Inter, system-ui, sans-serif", "$type": "fontFamily" },
          "heading": { "$value": "Inter, system-ui, sans-serif", "$type": "fontFamily" },
          "mono": { "$value": "ui-monospace, SFMono-Regular, Menlo, monospace", "$type": "fontFamily" }
        },
        "size": {
          "sm": { "$value": "0.875rem", "$type": "dimension" },
          "base": { "$value": "1rem", "$type": "dimension" },
          "lg": { "$value": "1.125rem", "$type": "dimension" },
          "xl": { "$value": "1.375rem", "$type": "dimension" }
        },
        "weight": {
          "normal": { "$value": 400, "$type": "fontWeight" },
          "bold": { "$value": 700, "$type": "fontWeight" }
        }
      },
      "radius": {
        "sm": { "$value": "0.25rem", "$type": "dimension" },
        "md": { "$value": "0.5rem", "$type": "dimension" },
        "lg": { "$value": "0.75rem", "$type": "dimension" },
        "full": { "$value": "9999px", "$type": "dimension" }
      },
      "spacing": {
        "scale": { "$value": 1, "$type": "number" },
        "unit": { "$value": "0.25rem", "$type": "dimension" }
      }
    }
  }',
  'artifacts/base-1.0.0.zip',
  '0000000000000000000000000000000000000000000000000000000000000000'
);

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the NOBYPASSRLS application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA theming TO sanvi_app;

GRANT SELECT, INSERT, UPDATE ON theming.themes TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON theming.theme_versions TO sanvi_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON theming.tenant_themes TO sanvi_app;
GRANT SELECT, INSERT ON theming.tenant_theme_revisions TO sanvi_app;
