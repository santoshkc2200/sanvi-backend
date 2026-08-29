-- 10.6: audience sync (customer-list build and scheduled refresh,
-- tracking consent for sale_or_share and targeted_advertising).
CREATE TABLE advertising.audiences (
  id                    bigint PRIMARY KEY,
  tenant_id             bigint NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  platform              text NOT NULL,
  name                  text NOT NULL,
  status                text NOT NULL, -- 'building' | 'active' | 'failed'
  failure_reason        text,
  external_ref          text,
  included_identifiers  jsonb NOT NULL DEFAULT '[]',
  last_synced_at        timestamptz,
  created_at            timestamptz NOT NULL DEFAULT now(),
  updated_at            timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX idx_advertising_audiences_tenant_id ON advertising.audiences(tenant_id, created_at DESC);
CREATE INDEX idx_advertising_audiences_due_refresh ON advertising.audiences(last_synced_at);

SELECT platform.enable_tenant_rls('advertising', 'audiences');
GRANT SELECT, INSERT, UPDATE ON advertising.audiences TO sanvi_app;
