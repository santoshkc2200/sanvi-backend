-- A receipt and its audit-chain append commit together. Remote delivery may retry.
-- Keyed by the service's audit command ID; the digest detects a reused ID.
CREATE TABLE audit.domains_receipts (
    command_id bigint PRIMARY KEY CHECK (command_id > 0),
    digest text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    received_at timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT, INSERT ON audit.domains_receipts TO sanvi_app;
