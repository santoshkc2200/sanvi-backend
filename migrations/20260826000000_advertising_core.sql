-- Phase 10.0: advertising contract, schema and safety foundations (TASK-009).
--
-- Partitioned parents deliberately have an explicit child-policy routine.
-- PostgreSQL's parent policy is not a substitute for proving every partition
-- has RLS enabled and forced; `ensure_monthly_partitions` performs both.

CREATE SCHEMA IF NOT EXISTS advertising;

CREATE TABLE advertising.connections (
  id                  uuid        PRIMARY KEY,
  tenant_id           uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  platform            text        NOT NULL CHECK (platform IN ('google_ads', 'meta')),
  external_account_id text        NOT NULL,
  account_name        text,
  currency            text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  timezone            text        NOT NULL,
  status              text        NOT NULL CHECK (status IN ('active', 'disconnected', 'expired')),
  scopes              text[]      NOT NULL DEFAULT '{}',
  token_ref           text        NOT NULL,
  connected_by        uuid,
  connected_at        timestamptz NOT NULL DEFAULT now(),
  last_synced_at      timestamptz,
  last_error          jsonb,
  created_at          timestamptz NOT NULL DEFAULT now(),
  updated_at          timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, id),
  UNIQUE (tenant_id, platform, id),
  UNIQUE (tenant_id, platform, external_account_id)
);
SELECT platform.enable_tenant_rls('advertising', 'connections');

CREATE TABLE advertising.campaigns (
  id            uuid        PRIMARY KEY,
  tenant_id     uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  connection_id uuid        NOT NULL,
  platform      text        NOT NULL CHECK (platform IN ('google_ads', 'meta')),
  external_id   text,
  name          text        NOT NULL,
  objective     text        NOT NULL,
  status        text        NOT NULL,
  budget        jsonb       NOT NULL,
  schedule      jsonb,
  drift         jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, id),
  UNIQUE (tenant_id, platform, id),
  UNIQUE (connection_id, external_id),
  FOREIGN KEY (tenant_id, platform, connection_id)
    REFERENCES advertising.connections(tenant_id, platform, id) ON DELETE CASCADE
);
SELECT platform.enable_tenant_rls('advertising', 'campaigns');

CREATE TABLE advertising.ad_groups (
  id          uuid        PRIMARY KEY,
  tenant_id   uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  campaign_id uuid        NOT NULL,
  external_id text,
  name        text        NOT NULL,
  targeting   jsonb       NOT NULL DEFAULT '{}',
  bid         jsonb,
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, id),
  UNIQUE (campaign_id, external_id),
  FOREIGN KEY (tenant_id, campaign_id)
    REFERENCES advertising.campaigns(tenant_id, id) ON DELETE CASCADE
);
SELECT platform.enable_tenant_rls('advertising', 'ad_groups');

CREATE TABLE advertising.creatives (
  id          uuid        PRIMARY KEY,
  tenant_id   uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  external_id text,
  specification jsonb     NOT NULL DEFAULT '{}',
  created_at  timestamptz NOT NULL DEFAULT now(),
  updated_at  timestamptz NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, id)
);
SELECT platform.enable_tenant_rls('advertising', 'creatives');

CREATE TABLE advertising.ads (
  id                uuid        PRIMARY KEY,
  tenant_id         uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  ad_group_id       uuid        NOT NULL,
  creative_id       uuid,
  external_id       text,
  landing_url       text        NOT NULL,
  tracking_template text,
  review_status     text,
  created_at        timestamptz NOT NULL DEFAULT now(),
  updated_at        timestamptz NOT NULL DEFAULT now(),
  UNIQUE (ad_group_id, external_id),
  FOREIGN KEY (tenant_id, ad_group_id)
    REFERENCES advertising.ad_groups(tenant_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, creative_id)
    REFERENCES advertising.creatives(tenant_id, id) ON DELETE SET NULL
);
SELECT platform.enable_tenant_rls('advertising', 'ads');

-- Global uniqueness on a PostgreSQL partitioned table must contain the
-- partition key. This compact tenant-scoped registry provides the actual
-- `(tenant_id, event_id)` deduplication guarantee without weakening monthly
-- partitioning of the event history.
CREATE TABLE advertising.conversion_event_dedup (
  tenant_id   uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  event_id    text        NOT NULL,
  occurred_at timestamptz NOT NULL,
  PRIMARY KEY (tenant_id, event_id)
);
SELECT platform.enable_tenant_rls('advertising', 'conversion_event_dedup');

CREATE TABLE advertising.conversion_events (
  id            uuid        NOT NULL,
  tenant_id     uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  event_id      text        NOT NULL,
  name          text        NOT NULL,
  occurred_at   timestamptz NOT NULL,
  value_minor   bigint,
  currency      text        CHECK (currency IS NULL OR currency ~ '^[A-Z]{3}$'),
  order_ref     text,
  click_ids     jsonb       NOT NULL DEFAULT '{}',
  hashed_ids    jsonb       NOT NULL DEFAULT '{}',
  consent       jsonb       NOT NULL,
  upload_state  jsonb       NOT NULL DEFAULT '{}',
  created_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (id, occurred_at),
  -- Named explicitly: PostgreSQL 11 derives a foreign-key constraint name from
  -- the *first* key column only, so an auto-named composite key here would
  -- collide with `conversion_events_tenant_id_fkey` above and land as
  -- `…_fkey1`, while 12+ would name it `…_tenant_id_event_id_fkey`. Later
  -- migrations have to drop this by name; the name cannot be server-dependent.
  CONSTRAINT conversion_events_tenant_id_event_id_fkey
    FOREIGN KEY (tenant_id, event_id) REFERENCES advertising.conversion_event_dedup(tenant_id, event_id)
) PARTITION BY RANGE (occurred_at);
SELECT platform.enable_tenant_rls('advertising', 'conversion_events');

CREATE TABLE advertising.metrics_daily (
  tenant_id                uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  platform                 text        NOT NULL CHECK (platform IN ('google_ads', 'meta')),
  connection_id            uuid        NOT NULL,
  campaign_id              uuid        NOT NULL,
  date                     date        NOT NULL,
  impressions              bigint      NOT NULL DEFAULT 0 CHECK (impressions >= 0),
  clicks                   bigint      NOT NULL DEFAULT 0 CHECK (clicks >= 0),
  spend_minor              bigint      NOT NULL DEFAULT 0 CHECK (spend_minor >= 0),
  currency                 text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  conversions              numeric     NOT NULL DEFAULT 0 CHECK (conversions >= 0),
  conversion_value_minor   bigint      NOT NULL DEFAULT 0 CHECK (conversion_value_minor >= 0),
  sanvi_revenue_minor      bigint      NOT NULL DEFAULT 0 CHECK (sanvi_revenue_minor >= 0),
  ingested_at              timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, platform, campaign_id, date),
  FOREIGN KEY (tenant_id, platform, connection_id)
    REFERENCES advertising.connections(tenant_id, platform, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, platform, campaign_id)
    REFERENCES advertising.campaigns(tenant_id, platform, id) ON DELETE CASCADE
) PARTITION BY RANGE (date);
SELECT platform.enable_tenant_rls('advertising', 'metrics_daily');

-- Invoked by the leader-elected worker every day. Security definer keeps DDL
-- privileges out of the NOBYPASSRLS application role; fixed search_path and
-- generated-only names avoid SQL injection through its date argument.
CREATE OR REPLACE FUNCTION advertising.ensure_monthly_partitions(target_date date)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, advertising
AS $$
DECLARE
  month_start date := date_trunc('month', target_date)::date;
  month_end date := (month_start + interval '1 month')::date;
  suffix text := to_char(month_start, 'YYYYMM');
  conversion_child text := format('conversion_events_%s', suffix);
  metrics_child text := format('metrics_daily_%s', suffix);
BEGIN
  EXECUTE format(
    'CREATE TABLE IF NOT EXISTS advertising.%I PARTITION OF advertising.conversion_events FOR VALUES FROM (%L) TO (%L)',
    conversion_child, month_start, month_end
  );
  EXECUTE format('ALTER TABLE advertising.%I ENABLE ROW LEVEL SECURITY', conversion_child);
  EXECUTE format('ALTER TABLE advertising.%I FORCE ROW LEVEL SECURITY', conversion_child);
  EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON advertising.%I', conversion_child);
  EXECUTE format(
    'CREATE POLICY tenant_isolation ON advertising.%I USING (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::uuid) WITH CHECK (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::uuid)',
    conversion_child
  );
  EXECUTE format('GRANT SELECT, INSERT, UPDATE ON advertising.%I TO sanvi_app', conversion_child);

  EXECUTE format(
    'CREATE TABLE IF NOT EXISTS advertising.%I PARTITION OF advertising.metrics_daily FOR VALUES FROM (%L) TO (%L)',
    metrics_child, month_start, month_end
  );
  EXECUTE format('ALTER TABLE advertising.%I ENABLE ROW LEVEL SECURITY', metrics_child);
  EXECUTE format('ALTER TABLE advertising.%I FORCE ROW LEVEL SECURITY', metrics_child);
  EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON advertising.%I', metrics_child);
  EXECUTE format(
    'CREATE POLICY tenant_isolation ON advertising.%I USING (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::uuid) WITH CHECK (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::uuid)',
    metrics_child
  );
  EXECUTE format('GRANT SELECT, INSERT, UPDATE ON advertising.%I TO sanvi_app', metrics_child);
END;
$$;

-- SECURITY DEFINER functions are executable by PUBLIC unless explicitly
-- revoked. Partition DDL is reserved for the application role only.
REVOKE ALL ON FUNCTION advertising.ensure_monthly_partitions(date) FROM PUBLIC;

SELECT advertising.ensure_monthly_partitions(current_date);
SELECT advertising.ensure_monthly_partitions((current_date + interval '1 month')::date);

GRANT USAGE ON SCHEMA advertising TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON advertising.connections, advertising.campaigns,
  advertising.ad_groups, advertising.ads, advertising.creatives,
  advertising.conversion_event_dedup, advertising.conversion_events,
  advertising.metrics_daily TO sanvi_app;
GRANT EXECUTE ON FUNCTION advertising.ensure_monthly_partitions(date) TO sanvi_app;

-- Retention is registered before the first event can land. These classes are
-- declared in the phase-05 data map so notices and retention sweeps parse
-- every rule loaded from the operational registry.
INSERT INTO privacy.retention_rules
  (data_class, sensitivity, basis, period, action, disclosed_in_notice, updated_by)
VALUES
  ('advertising_conversion_event', 'sensitive', 'consent', make_interval(days => 730), 'anonymise', true, '00000000-0000-0000-0000-000000000100'),
  ('advertising_metric', 'standard', 'legitimate_interest', make_interval(days => 1095), 'delete', true, '00000000-0000-0000-0000-000000000100')
ON CONFLICT (data_class) DO NOTHING;
