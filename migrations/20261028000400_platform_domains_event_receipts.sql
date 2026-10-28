-- Remote outbox receipt and local publication commit together.
CREATE TABLE platform.domains_event_receipts (
    event_id bigint PRIMARY KEY CHECK (event_id > 0),
    digest text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    received_at timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT, INSERT ON platform.domains_event_receipts TO sanvi_app;
