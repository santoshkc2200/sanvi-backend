-- TASK-011: durable, tenant-bound OAuth state nonces. A callback consumes a
-- row exactly once; retaining redeemed rows makes replay attempts auditable.
CREATE TABLE advertising.oauth_states (
  nonce      uuid        PRIMARY KEY,
  tenant_id  uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  platform   text        NOT NULL CHECK (platform IN ('google_ads', 'meta')),
  expires_at timestamptz NOT NULL,
  redeemed_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX advertising_oauth_states_expiry_idx
  ON advertising.oauth_states (expires_at)
  WHERE redeemed_at IS NULL;
SELECT platform.enable_tenant_rls('advertising', 'oauth_states');
GRANT SELECT, INSERT, UPDATE ON advertising.oauth_states TO sanvi_app;
