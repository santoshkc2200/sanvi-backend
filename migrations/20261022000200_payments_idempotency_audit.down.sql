DROP INDEX IF EXISTS payments.refunds_pending_audit_idx;
ALTER TABLE payments.refunds
    DROP COLUMN audited_at,
    DROP COLUMN audit_ready,
    DROP COLUMN audit_request;
ALTER TABLE payments.checkouts DROP COLUMN request_fingerprint;
