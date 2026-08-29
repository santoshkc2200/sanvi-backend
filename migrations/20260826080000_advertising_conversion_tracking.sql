-- 10.5: tracking settings (event-name -> per-platform conversion action
-- mapping) and the value-source label on captured conversion events, so a
-- payment-record value and a client-reported hint are never confused.
CREATE TABLE advertising.tracking_settings (
  tenant_id  bigint        PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  mappings   jsonb       NOT NULL DEFAULT '{}',
  updated_at timestamptz NOT NULL DEFAULT now()
);
SELECT platform.enable_tenant_rls('advertising', 'tracking_settings');
GRANT SELECT, INSERT, UPDATE ON advertising.tracking_settings TO sanvi_app;

ALTER TABLE advertising.conversion_events
  ADD COLUMN value_source text CHECK (value_source IN ('payment_record', 'client_reported'));
