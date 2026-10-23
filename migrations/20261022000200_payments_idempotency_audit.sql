-- Expand only: old binaries ignore the added nullable/defaulted columns.
ALTER TABLE payments.checkouts
    ADD COLUMN request_fingerprint text
    CHECK (request_fingerprint IS NULL OR request_fingerprint ~ '^[a-f0-9]{64}$');

-- Keep attribution on the existing refund retention/RLS/offboarding boundary.
-- No FK to a user: deleting a user must not erase financial attribution.
ALTER TABLE payments.refunds
    ADD COLUMN audit_request jsonb,
    ADD COLUMN audit_ready boolean NOT NULL DEFAULT false,
    ADD COLUMN audited_at timestamptz;
CREATE INDEX refunds_pending_audit_idx ON payments.refunds (created_at, id)
    WHERE audit_ready AND audited_at IS NULL;
