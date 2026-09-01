-- Phase 10.8: budget guardrails and spend alerts (TASK-017).

CREATE TABLE advertising.budget_caps (
  id                        bigint        PRIMARY KEY,
  tenant_id                 bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  campaign_id               bigint,
  period                    text          NOT NULL CHECK (period IN ('daily', 'monthly')),
  amount_minor              bigint        NOT NULL CHECK (amount_minor > 0),
  currency                  text          NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  declared_fx_basis         text,
  fx_rate_date              date,
  auto_pause                boolean       NOT NULL DEFAULT false,
  auto_resume_on_rollover   boolean       NOT NULL DEFAULT false,
  version                   bigint        NOT NULL DEFAULT 1 CHECK (version > 0),
  created_at                timestamptz   NOT NULL DEFAULT now(),
  updated_at                timestamptz   NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, id),
  FOREIGN KEY (tenant_id, campaign_id)
    REFERENCES advertising.campaigns(tenant_id, id) ON DELETE CASCADE
);
SELECT platform.enable_tenant_rls('advertising', 'budget_caps');

CREATE UNIQUE INDEX budget_caps_tenant_period_idx
  ON advertising.budget_caps (tenant_id, period)
  WHERE campaign_id IS NULL;

CREATE UNIQUE INDEX budget_caps_campaign_period_idx
  ON advertising.budget_caps (tenant_id, campaign_id, period)
  WHERE campaign_id IS NOT NULL;

CREATE TABLE advertising.budget_alerts (
  id                        bigint        PRIMARY KEY,
  tenant_id                 bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  cap_id                    bigint,
  campaign_id               bigint,
  period                    text          NOT NULL CHECK (period IN ('daily', 'monthly')),
  period_start              date          NOT NULL,
  threshold                 integer       NOT NULL CHECK (threshold IN (80, 100)),
  condition                 text          NOT NULL,
  spend_minor               bigint        NOT NULL CHECK (spend_minor >= 0),
  cap_minor                 bigint        NOT NULL CHECK (cap_minor >= 0),
  currency                  text          NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  data_freshness            jsonb         NOT NULL DEFAULT '{}'::jsonb,
  auto_paused               boolean       NOT NULL DEFAULT false,
  acknowledged_at           timestamptz,
  acknowledged_by           bigint,
  created_at                timestamptz   NOT NULL DEFAULT now(),
  UNIQUE (tenant_id, id),
  FOREIGN KEY (tenant_id, cap_id)
    REFERENCES advertising.budget_caps(tenant_id, id) ON DELETE CASCADE,
  FOREIGN KEY (tenant_id, campaign_id)
    REFERENCES advertising.campaigns(tenant_id, id) ON DELETE CASCADE
);
SELECT platform.enable_tenant_rls('advertising', 'budget_alerts');

CREATE UNIQUE INDEX budget_alerts_dedup_idx
  ON advertising.budget_alerts (tenant_id, cap_id, period_start, threshold);

ALTER TABLE advertising.campaign_changes
  ADD COLUMN IF NOT EXISTS attribution text,
  ADD COLUMN IF NOT EXISTS metadata jsonb;

GRANT SELECT, INSERT, UPDATE, DELETE ON advertising.budget_caps TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON advertising.budget_alerts TO sanvi_app;

INSERT INTO privacy.retention_rules
  (data_class, sensitivity, basis, period, action, disclosed_in_notice, updated_by)
VALUES
  ('advertising_budget_cap', 'standard', 'legitimate_interest',
   make_interval(days => 1095), 'delete', true, '100'),
  ('advertising_budget_alert', 'standard', 'legitimate_interest',
   make_interval(days => 1095), 'delete', true, '100')
ON CONFLICT (data_class) DO NOTHING;
