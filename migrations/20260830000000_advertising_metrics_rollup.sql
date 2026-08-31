-- 10.7: metrics ingestion, rollups & ROAS query layer (TASK-016).
--
-- `metrics_daily` (TASK-009) already carries the campaign grain. Revenue
-- that cannot be attributed to exactly one campaign for a given
-- tenant/platform/day/currency is never distributed across candidate
-- campaigns by a guess (NFR-1006) — it lands here instead, surfaced only in
-- platform- and tenant-grain queries. Currency is part of the identity, not
-- just a stored column: a tenant can run more than one ad account on the
-- same platform in different currencies, and each currency's ambiguous
-- revenue for a day needs its own row rather than colliding on one.
CREATE TABLE advertising.metrics_unattributed_revenue_daily (
  tenant_id     bigint      NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  platform      text        NOT NULL,
  date          date        NOT NULL,
  currency      text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  revenue_minor bigint      NOT NULL DEFAULT 0 CHECK (revenue_minor >= 0),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (tenant_id, platform, date, currency)
);
SELECT platform.enable_tenant_rls('advertising', 'metrics_unattributed_revenue_daily');
GRANT SELECT, INSERT, UPDATE, DELETE ON advertising.metrics_unattributed_revenue_daily TO sanvi_app;

-- `metrics_daily`'s primary key leads with `campaign_id`, which does not
-- serve a platform- or tenant-grain scan over a date range. This index does;
-- created without CONCURRENTLY like every other index in this migration set,
-- consistent with how the phase's earlier migrations size this table today.
CREATE INDEX idx_advertising_metrics_daily_tenant_platform_date
  ON advertising.metrics_daily (tenant_id, platform, date);
