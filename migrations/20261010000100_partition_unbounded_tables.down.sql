-- Down: rebuild the four unbounded tables in their original
-- unpartitioned shapes. The partitioned parents (and their children) are
-- dropped after a count-verified copy back; the webhook dedup sidecars
-- are dropped last, after the history rows that reference them.

-- --- payments.webhook_events -----------------------------------------------
DO $$
DECLARE
  old_count bigint;
  new_count bigint;
BEGIN
  CREATE TABLE payments.webhook_events_down (
    stripe_event_id   text        PRIMARY KEY,
    account_id        text,
    type              text        NOT NULL,
    payload           jsonb       NOT NULL,
    api_version       text,
    received_at       timestamptz NOT NULL DEFAULT now(),
    processed_at      timestamptz,
    attempts          int         NOT NULL DEFAULT 0,
    error             text,
    created           boolean     NOT NULL DEFAULT false
  );

  INSERT INTO payments.webhook_events_down
    (stripe_event_id, account_id, type, payload, api_version,
     received_at, processed_at, attempts, error, created)
  SELECT stripe_event_id, account_id, type, payload, api_version,
         received_at, processed_at, attempts, error, created
  FROM payments.webhook_events;

  SELECT count(*) INTO old_count FROM payments.webhook_events;
  SELECT count(*) INTO new_count FROM payments.webhook_events_down;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'payments.webhook_events down-conversion lost rows: % -> %', old_count, new_count;
  END IF;

  DELETE FROM platform.partitioned_tables
  WHERE schema_name = 'payments' AND table_name = 'webhook_events';

  ALTER TABLE payments.webhook_events RENAME TO webhook_events_p_dropped;
  ALTER TABLE payments.webhook_events_down RENAME TO webhook_events;
  DROP TABLE payments.webhook_events_p_dropped CASCADE;
  DROP TABLE payments.webhook_event_dedup;

  CREATE INDEX webhook_events_unprocessed_idx
    ON payments.webhook_events (received_at)
    WHERE processed_at IS NULL AND attempts < 5;
  CREATE INDEX webhook_events_account_idx ON payments.webhook_events (account_id);
  GRANT SELECT, INSERT, UPDATE ON payments.webhook_events TO sanvi_app;
END;
$$;

-- --- billing.webhook_events --------------------------------------------------
DO $$
DECLARE
  old_count bigint;
  new_count bigint;
BEGIN
  CREATE TABLE billing.webhook_events_down (
    stripe_event_id text        PRIMARY KEY,
    type            text        NOT NULL,
    received_at     timestamptz NOT NULL DEFAULT now(),
    processed_at    timestamptz,
    api_version     text,
    payload         jsonb       NOT NULL,
    attempts        int         NOT NULL DEFAULT 0,
    error           text,
    created         timestamptz NOT NULL
  );

  INSERT INTO billing.webhook_events_down
    (stripe_event_id, type, received_at, processed_at, api_version,
     payload, attempts, error, created)
  SELECT stripe_event_id, type, received_at, processed_at, api_version,
         payload, attempts, error, created
  FROM billing.webhook_events;

  SELECT count(*) INTO old_count FROM billing.webhook_events;
  SELECT count(*) INTO new_count FROM billing.webhook_events_down;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'billing.webhook_events down-conversion lost rows: % -> %', old_count, new_count;
  END IF;

  DELETE FROM platform.partitioned_tables
  WHERE schema_name = 'billing' AND table_name = 'webhook_events';

  ALTER TABLE billing.webhook_events RENAME TO webhook_events_p_dropped;
  ALTER TABLE billing.webhook_events_down RENAME TO webhook_events;
  DROP TABLE billing.webhook_events_p_dropped CASCADE;
  DROP TABLE billing.webhook_event_dedup;

  CREATE INDEX webhook_events_unprocessed_idx
    ON billing.webhook_events (received_at)
    WHERE processed_at IS NULL AND attempts < 5;
  GRANT SELECT, INSERT, UPDATE ON billing.webhook_events TO sanvi_app;
END;
$$;

-- --- platform.outbox ---------------------------------------------------------
DO $$
DECLARE
  old_count bigint;
  new_count bigint;
BEGIN
  CREATE TABLE platform.outbox_down (
    id            bigint PRIMARY KEY,
    aggregate     text        NOT NULL,
    aggregate_id  bigint      NOT NULL,
    event_type    text        NOT NULL,
    payload       jsonb       NOT NULL,
    metadata      jsonb       NOT NULL DEFAULT '{}',
    occurred_at   timestamptz NOT NULL DEFAULT now(),
    published_at  timestamptz,
    attempts      int         NOT NULL DEFAULT 0
  );

  INSERT INTO platform.outbox_down
    (id, aggregate, aggregate_id, event_type, payload, metadata,
     occurred_at, published_at, attempts)
  SELECT id, aggregate, aggregate_id, event_type, payload, metadata,
         occurred_at, published_at, attempts
  FROM platform.outbox;

  SELECT count(*) INTO old_count FROM platform.outbox;
  SELECT count(*) INTO new_count FROM platform.outbox_down;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'platform.outbox down-conversion lost rows: % -> %', old_count, new_count;
  END IF;

  DELETE FROM platform.partitioned_tables
  WHERE schema_name = 'platform' AND table_name = 'outbox';

  ALTER TABLE platform.outbox RENAME TO outbox_p_dropped;
  ALTER TABLE platform.outbox_down RENAME TO outbox;
  DROP TABLE platform.outbox_p_dropped;

  CREATE INDEX outbox_unpublished_idx
    ON platform.outbox (published_at NULLS FIRST, occurred_at);
  GRANT SELECT, INSERT, UPDATE ON platform.outbox TO sanvi_app;
END;
$$;

-- --- audit.entries -----------------------------------------------------------
DO $$
DECLARE
  old_count bigint;
  new_count bigint;
BEGIN
  CREATE TABLE audit.entries_down (
    seq           bigint       NOT NULL DEFAULT nextval('audit.entries_seq_seq'),
    id            bigint      PRIMARY KEY,
    occurred_at   timestamptz NOT NULL DEFAULT now(),
    actor_type    text         NOT NULL,
    actor_id      bigint,
    on_behalf_of  bigint,
    tenant_id     bigint,
    action        text         NOT NULL,
    resource_type text,
    resource_id   text,
    before        jsonb,
    after         jsonb,
    request_id    text,
    ip            inet,
    user_agent    text,
    prev_hash     bytea        NOT NULL,
    hash          bytea        NOT NULL,
    CONSTRAINT audit_actor_type_check_down
      CHECK (actor_type IN ('user', 'platform_admin', 'system')),
    CONSTRAINT audit_action_nonempty_down CHECK (length(btrim(action)) > 0)
  );

  INSERT INTO audit.entries_down
    (seq, id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
     action, resource_type, resource_id, before, after, request_id,
     ip, user_agent, prev_hash, hash)
  SELECT seq, id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
         action, resource_type, resource_id, before, after, request_id,
         ip, user_agent, prev_hash, hash
  FROM audit.entries;

  SELECT count(*) INTO old_count FROM audit.entries;
  SELECT count(*) INTO new_count FROM audit.entries_down;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'audit.entries down-conversion lost rows: % -> %', old_count, new_count;
  END IF;

  DELETE FROM platform.partitioned_tables
  WHERE schema_name = 'audit' AND table_name = 'entries';

  ALTER TABLE audit.entries RENAME TO entries_p_dropped;
  ALTER TABLE audit.entries_down RENAME TO entries;
  -- The sequence is OWNED BY the partitioned table's column; re-point it
  -- or the drop refuses.
  ALTER SEQUENCE audit.entries_seq_seq OWNED BY audit.entries.seq;
  DROP TABLE audit.entries_p_dropped;

  CREATE UNIQUE INDEX audit_entries_seq_unique ON audit.entries (seq);
  CREATE INDEX audit_entries_occurred_at_idx ON audit.entries (occurred_at DESC);
  CREATE INDEX audit_entries_tenant_idx ON audit.entries (tenant_id, occurred_at DESC);
  CREATE INDEX audit_entries_actor_idx ON audit.entries (actor_id, occurred_at DESC);
  CREATE INDEX audit_entries_action_idx ON audit.entries (action, occurred_at DESC);
  CREATE INDEX audit_entries_tenant_seq_idx ON audit.entries (tenant_id, seq);
  ALTER TABLE audit.entries RENAME CONSTRAINT audit_actor_type_check_down TO audit_actor_type_check;
  ALTER TABLE audit.entries RENAME CONSTRAINT audit_action_nonempty_down TO audit_action_nonempty;

  PERFORM platform.enable_tenant_rls('audit', 'entries');
  CREATE TRIGGER entries_no_update
    BEFORE UPDATE OR DELETE ON audit.entries
    FOR EACH ROW
    EXECUTE FUNCTION audit.entries_append_only();
  GRANT SELECT, INSERT ON audit.entries TO sanvi_app;
END;
$$;
