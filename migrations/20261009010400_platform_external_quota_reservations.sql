-- External services cannot join our transaction. Reserve before dispatch and
-- retain ambiguous outcomes; retries with the same key never reserve twice.
CREATE TABLE platform.external_quota_reservations (
  tenant_id bigint NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  dimension text NOT NULL CHECK (dimension IN ('storage_bytes','notification_sends')),
  key_hash text NOT NULL,
  amount bigint NOT NULL CHECK (amount > 0),
  period_start date NOT NULL,
  asset_id text,
  released boolean NOT NULL DEFAULT false,
  payload_hash text CHECK (payload_hash IS NULL OR length(payload_hash)=64),
  accepted boolean NOT NULL DEFAULT false,
  ambiguous boolean NOT NULL DEFAULT false,
  next_attempt bigint NOT NULL DEFAULT 0,
  active_attempts bigint[] NOT NULL DEFAULT '{}'
    CHECK (cardinality(active_attempts)<=128),
  PRIMARY KEY(tenant_id,dimension,key_hash),
  UNIQUE(tenant_id,dimension,asset_id)
);
SELECT platform.enable_tenant_rls('platform','external_quota_reservations');
GRANT SELECT,INSERT,UPDATE,DELETE ON platform.external_quota_reservations TO sanvi_app;
