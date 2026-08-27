-- Phase 10.3: campaign intent, remote snapshots, and attributed changes.
--
-- `intent` and `remote_snapshot` must remain separate.  Reconciliation is a
-- three-way comparison (intent, last remote state, current remote state), so
-- storing only the latest campaign would turn native platform edits into
-- destructive write-backs.

ALTER TABLE advertising.campaigns
  ADD COLUMN revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
  ADD COLUMN intent jsonb NOT NULL DEFAULT '{}'::jsonb,
  ADD COLUMN remote_snapshot jsonb NOT NULL DEFAULT '{}'::jsonb;

CREATE TABLE advertising.campaign_changes (
  id bigint PRIMARY KEY,
  tenant_id bigint NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  campaign_id bigint NOT NULL,
  revision bigint NOT NULL CHECK (revision > 0),
  mutation_id bigint,
  actor_id bigint,
  source text NOT NULL CHECK (source IN ('sanvi', 'platform')),
  changed_fields jsonb NOT NULL DEFAULT '[]'::jsonb,
  before_state jsonb NOT NULL DEFAULT '{}'::jsonb,
  after_state jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at timestamptz NOT NULL DEFAULT now(),
  FOREIGN KEY (tenant_id, campaign_id)
    REFERENCES advertising.campaigns(tenant_id, id) ON DELETE CASCADE
);
SELECT platform.enable_tenant_rls('advertising', 'campaign_changes');

CREATE INDEX campaign_changes_campaign_created_idx
  ON advertising.campaign_changes (tenant_id, campaign_id, created_at DESC);
-- A partial index works on PostgreSQL versions before `NULLS NOT DISTINCT`.
-- Platform-sourced observations have no mutation id and must stay append-only.
CREATE UNIQUE INDEX campaign_changes_mutation_id_unique
  ON advertising.campaign_changes (tenant_id, campaign_id, mutation_id)
  WHERE mutation_id IS NOT NULL;

GRANT SELECT, INSERT ON advertising.campaign_changes TO sanvi_app;

INSERT INTO privacy.retention_rules
  (data_class, sensitivity, basis, period, action, disclosed_in_notice, updated_by)
VALUES
  ('advertising_campaign_change', 'standard', 'legitimate_interest',
   make_interval(days => 1095), 'delete', true,
   '100')
ON CONFLICT (data_class) DO NOTHING;
