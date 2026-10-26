-- Provider OAuth consent for accounting connections. Token material is sealed
-- with a per-tenant DEK, itself wrapped by a KEK held outside the database.
-- Keys are per context: privacy erasure or advertising rotation never reaches
-- an accounting credential.

-- Single-use consent state. Redeemed rows are retained so replays are visible.
CREATE TABLE accounting.oauth_states (
    nonce uuid PRIMARY KEY,
    tenant_id bigint NOT NULL CHECK (tenant_id > 0),
    user_id bigint NOT NULL CHECK (user_id > 0),
    book_id bigint NOT NULL,
    connection_id bigint NOT NULL CHECK (connection_id > 0),
    provider text NOT NULL CHECK (provider IN ('freee', 'money_forward', 'xero')),
    expires_at timestamptz NOT NULL,
    redeemed_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant_id, book_id) REFERENCES accounting.books (tenant_id, id)
);
CREATE INDEX oauth_states_unredeemed ON accounting.oauth_states (expires_at) WHERE redeemed_at IS NULL;
SELECT platform.enable_tenant_rls('accounting', 'oauth_states');

CREATE TABLE accounting.tenant_secret_keys (
    tenant_id bigint PRIMARY KEY CHECK (tenant_id > 0),
    wrapped_dek bytea NOT NULL,
    kek_id text NOT NULL CHECK (length(kek_id) BETWEEN 1 AND 64),
    created_at timestamptz NOT NULL DEFAULT now()
);
SELECT platform.enable_tenant_rls('accounting', 'tenant_secret_keys');

-- One row per connection binding. Only an active credential holds tokens.
CREATE TABLE accounting.provider_credentials (
    connection_id bigint PRIMARY KEY CHECK (connection_id > 0),
    tenant_id bigint NOT NULL CHECK (tenant_id > 0),
    book_id bigint NOT NULL,
    status text NOT NULL CHECK (status IN ('active', 'reauthorization_required', 'revoked')),
    refresh_ciphertext bytea,
    refresh_issued_at timestamptz,
    access_ciphertext bytea,
    access_expires_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (tenant_id, book_id) REFERENCES accounting.books (tenant_id, id),
    CHECK (
        (status = 'active' AND refresh_ciphertext IS NOT NULL AND refresh_issued_at IS NOT NULL
            AND access_ciphertext IS NOT NULL AND access_expires_at IS NOT NULL) OR
        (status <> 'active' AND refresh_ciphertext IS NULL AND refresh_issued_at IS NULL
            AND access_ciphertext IS NULL AND access_expires_at IS NULL)
    )
);
SELECT platform.enable_tenant_rls('accounting', 'provider_credentials');

-- No DELETE: revocation clears token columns and keeps the row.
GRANT SELECT, INSERT, UPDATE ON accounting.oauth_states, accounting.tenant_secret_keys, accounting.provider_credentials TO sanvi_app;
