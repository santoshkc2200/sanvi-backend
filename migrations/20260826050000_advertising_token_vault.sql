-- TASK-011: per-tenant envelope encryption for OAuth refresh tokens.
--
-- `advertising.connections.token_ref` is an opaque locator (the connection's
-- own id), never a credential. The credential material lives here, wrapped
-- with a tenant DEK, itself wrapped by a KEK held outside the database — the
-- same envelope pattern as `privacy.subject_keys`, keyed by tenant instead of
-- data subject so a subject erasure can never reach a live ad-account token.
CREATE TABLE advertising.tenant_secret_keys (
  tenant_id    uuid        PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  wrapped_dek  bytea       NOT NULL,
  kek_id       text        NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now()
);
SELECT platform.enable_tenant_rls('advertising', 'tenant_secret_keys');
GRANT SELECT, INSERT, UPDATE ON advertising.tenant_secret_keys TO sanvi_app;

CREATE TABLE advertising.connection_tokens (
  connection_id uuid        PRIMARY KEY REFERENCES advertising.connections(id) ON DELETE CASCADE,
  tenant_id     uuid        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  ciphertext    bytea       NOT NULL,
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now()
);
SELECT platform.enable_tenant_rls('advertising', 'connection_tokens');
GRANT SELECT, INSERT, UPDATE ON advertising.connection_tokens TO sanvi_app;
