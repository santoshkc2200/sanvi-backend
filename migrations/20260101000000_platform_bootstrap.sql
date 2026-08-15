-- platform.outbox: transactional outbox for domain events.
-- Every write path that emits events writes them here in the same
-- transaction as the state change; the worker relays unpublished rows to
-- Redis Streams.
CREATE SCHEMA IF NOT EXISTS platform;

CREATE TABLE platform.outbox (
  id            uuid PRIMARY KEY,
  aggregate     text        NOT NULL,
  aggregate_id  uuid        NOT NULL,
  event_type    text        NOT NULL,
  payload       jsonb       NOT NULL,
  metadata      jsonb       NOT NULL DEFAULT '{}',
  occurred_at   timestamptz NOT NULL DEFAULT now(),
  published_at  timestamptz,
  attempts      int         NOT NULL DEFAULT 0
);

CREATE INDEX outbox_unpublished_idx
  ON platform.outbox (published_at NULLS FIRST, occurred_at);

CREATE TABLE platform.idempotency_keys (
  key         text PRIMARY KEY,
  scope       text        NOT NULL,
  response    jsonb,
  status      text        NOT NULL,
  created_at  timestamptz NOT NULL DEFAULT now(),
  expires_at  timestamptz NOT NULL
);

-- Least-privilege grants: the API role may write to the outbox and take a
-- claim on unpublished rows, but it may not change the schema.
GRANT USAGE ON SCHEMA platform TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON platform.outbox TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON platform.idempotency_keys TO sanvi_app;
