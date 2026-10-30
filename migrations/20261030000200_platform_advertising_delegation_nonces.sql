CREATE TABLE platform.advertising_delegation_nonces (jti text PRIMARY KEY, expires_at timestamptz NOT NULL);
CREATE INDEX advertising_delegation_expiry ON platform.advertising_delegation_nonces (expires_at);
GRANT SELECT, INSERT, DELETE ON platform.advertising_delegation_nonces TO sanvi_app;
