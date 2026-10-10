-- TASK-028 (FR-1128): monthly range partitioning for the remaining
-- unbounded tables — audit.entries, platform.outbox, billing.webhook_events,
-- payments.webhook_events — from the one registry deployed in
-- 20261010000000_partition_registry.
--
-- expand-contract: this is a maintenance-window conversion, not a rolling
-- expand/contract: each table is rewritten (new partitioned shape,
-- count-verified copy, rename swap, drop of the old table) inside one
-- transaction, deployed with the code that reads the same column set —
-- the before/after shapes differ only in keys, and the deploy is
-- coordinated with the window (benchmarks/partition-conversion-2026-10-10.md
-- has the rehearsal procedure and the measured durations). The webhook
-- insert's ON CONFLICT moves to the claim-sidecar CTE in the same deploy.
--
-- This is the migration most likely to need a maintenance window: each
-- conversion rewrites a table that may already hold a hot tenant's history
-- (expand/contract, TASK-023's pattern: new partitioned shape, verified
-- copy, atomic swap, drop). Each conversion prints its duration as a
-- NOTICE; the scale-data rehearsal procedure and its measured timings are
-- recorded in benchmarks/partition-conversion-2026-10-10.md and must be
-- re-measured before scheduling against production.
--
-- Primary-key/global-uniqueness consequences, per table:
--   * audit.entries / platform.outbox: PK gains the range key
--     ((id, occurred_at)). `id` values come from the snowflake generator,
--     so uniqueness of id alone is preserved by construction; the unique
--     seq index becomes (seq, occurred_at) — the bigserial plus the
--     append advisory lock keep seq globally ordered either way.
--   * webhook tables: PK (stripe_event_id) cannot survive partitioning
--     (a unique index must contain the range key), so each gets the
--     phase-10 sidecar registry (webhook_event_dedup) that owns the
--     global claim, and the partitioned history gets PK
--     (stripe_event_id, received_at) with an FK into the sidecar. The
--     application insert moves to a claim-CTE in the same transaction as
--     this migration deploys.
--
-- Children inherit nothing: the registry's ensure function re-applies the
-- parent's RLS policy (audit only — the other three are platform-scoped
-- tables without tenant_id, by design since phase 04/09) and the parent's
-- sanvi_app grants on every child, by name.

-- ---------------------------------------------------------------------------
-- audit.entries
-- ---------------------------------------------------------------------------
DO $$
DECLARE
  started timestamptz := clock_timestamp();
  first_month date;
  m date;
  old_count bigint;
  new_count bigint;
BEGIN
  CREATE TABLE audit.entries_p (
    seq           bigint       NOT NULL DEFAULT nextval('audit.entries_seq_seq'),
    id            bigint       NOT NULL,
    occurred_at   timestamptz  NOT NULL DEFAULT now(),
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
    CONSTRAINT audit_actor_type_check_p
      CHECK (actor_type IN ('user', 'platform_admin', 'system')),
    CONSTRAINT audit_action_nonempty_p CHECK (length(btrim(action)) > 0),
    PRIMARY KEY (id, occurred_at)
  ) PARTITION BY RANGE (occurred_at);

  -- The registry's child-hardening copies this policy and these grants
  -- onto every child, so both must exist on the parent before it calls.
  PERFORM platform.enable_tenant_rls('audit', 'entries_p');
  GRANT SELECT, INSERT ON audit.entries_p TO sanvi_app;

  CREATE TRIGGER entries_no_update
    BEFORE UPDATE OR DELETE ON audit.entries_p
    FOR EACH ROW
    EXECUTE FUNCTION audit.entries_append_only();

  ALTER TABLE audit.entries RENAME TO entries_old;
  ALTER TABLE audit.entries_p RENAME TO entries;

  INSERT INTO platform.partitioned_tables (schema_name, table_name, key_column)
  VALUES ('audit', 'entries', 'occurred_at');

  SELECT date_trunc('month', min(occurred_at))::date INTO first_month FROM audit.entries_old;
  IF first_month IS NULL THEN
    first_month := date_trunc('month', current_date)::date;
  END IF;
  FOR m IN
    SELECT generate_series(first_month, date_trunc('month', current_date)::date,
                           interval '1 month')::date
  LOOP
    PERFORM platform.ensure_monthly_partitions(m);
  END LOOP;

  INSERT INTO audit.entries
    (seq, id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
     action, resource_type, resource_id, before, after, request_id,
     ip, user_agent, prev_hash, hash)
  SELECT seq, id, occurred_at, actor_type, actor_id, on_behalf_of, tenant_id,
         action, resource_type, resource_id, before, after, request_id,
         ip, user_agent, prev_hash, hash
  FROM audit.entries_old
  ORDER BY occurred_at, seq;

  SELECT count(*) INTO old_count FROM audit.entries_old;
  SELECT count(*) INTO new_count FROM audit.entries;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'audit.entries conversion lost rows: % -> %', old_count, new_count;
  END IF;

  -- The sequence is OWNED BY the old table's column; re-point it or the
  -- drop below refuses (and the app role's USAGE grant travels with the
  -- sequence, not the table).
  ALTER SEQUENCE audit.entries_seq_seq OWNED BY audit.entries.seq;

  DROP TABLE audit.entries_old;

  -- Global uniqueness including the partition key. The bigserial plus the
  -- append advisory lock remain the ordering authority; the unique index
  -- keeps its belt-and-braces role per partition.
  CREATE UNIQUE INDEX audit_entries_seq_unique ON audit.entries (seq, occurred_at);
  CREATE INDEX audit_entries_occurred_at_idx ON audit.entries (occurred_at DESC);
  CREATE INDEX audit_entries_tenant_idx ON audit.entries (tenant_id, occurred_at DESC);
  CREATE INDEX audit_entries_actor_idx ON audit.entries (actor_id, occurred_at DESC);
  CREATE INDEX audit_entries_action_idx ON audit.entries (action, occurred_at DESC);
  CREATE INDEX audit_entries_tenant_seq_idx ON audit.entries (tenant_id, seq);
  ALTER INDEX audit.entries_p_pkey RENAME TO entries_pkey;
  ALTER TABLE audit.entries RENAME CONSTRAINT audit_actor_type_check_p TO audit_actor_type_check;
  ALTER TABLE audit.entries RENAME CONSTRAINT audit_action_nonempty_p TO audit_action_nonempty;

  RAISE NOTICE 'audit.entries partition conversion: % rows in % ms',
    new_count, (clock_timestamp() - started);
END;
$$;

-- ---------------------------------------------------------------------------
-- platform.outbox
-- ---------------------------------------------------------------------------
DO $$
DECLARE
  started timestamptz := clock_timestamp();
  first_month date;
  m date;
  old_count bigint;
  new_count bigint;
BEGIN
  CREATE TABLE platform.outbox_p (
    id            bigint        NOT NULL,
    aggregate     text          NOT NULL,
    aggregate_id  bigint        NOT NULL,
    event_type    text          NOT NULL,
    payload       jsonb         NOT NULL,
    metadata      jsonb         NOT NULL DEFAULT '{}',
    occurred_at   timestamptz   NOT NULL DEFAULT now(),
    published_at  timestamptz,
    attempts      int           NOT NULL DEFAULT 0,
    PRIMARY KEY (id, occurred_at)
  ) PARTITION BY RANGE (occurred_at);

  GRANT SELECT, INSERT, UPDATE ON platform.outbox_p TO sanvi_app;

  ALTER TABLE platform.outbox RENAME TO outbox_old;
  ALTER TABLE platform.outbox_p RENAME TO outbox;

  INSERT INTO platform.partitioned_tables (schema_name, table_name, key_column)
  VALUES ('platform', 'outbox', 'occurred_at');

  SELECT date_trunc('month', min(occurred_at))::date INTO first_month FROM platform.outbox_old;
  IF first_month IS NULL THEN
    first_month := date_trunc('month', current_date)::date;
  END IF;
  FOR m IN
    SELECT generate_series(first_month, date_trunc('month', current_date)::date,
                           interval '1 month')::date
  LOOP
    PERFORM platform.ensure_monthly_partitions(m);
  END LOOP;

  INSERT INTO platform.outbox
    (id, aggregate, aggregate_id, event_type, payload, metadata,
     occurred_at, published_at, attempts)
  SELECT id, aggregate, aggregate_id, event_type, payload, metadata,
         occurred_at, published_at, attempts
  FROM platform.outbox_old
  ORDER BY occurred_at, id;

  SELECT count(*) INTO old_count FROM platform.outbox_old;
  SELECT count(*) INTO new_count FROM platform.outbox;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'platform.outbox conversion lost rows: % -> %', old_count, new_count;
  END IF;

  DROP TABLE platform.outbox_old;

  CREATE INDEX outbox_unpublished_idx
    ON platform.outbox (published_at NULLS FIRST, occurred_at);
  ALTER INDEX platform.outbox_p_pkey RENAME TO outbox_pkey;

  RAISE NOTICE 'platform.outbox partition conversion: % rows in % ms',
    new_count, (clock_timestamp() - started);
END;
$$;

-- ---------------------------------------------------------------------------
-- billing.webhook_events (+ dedup sidecar)
-- ---------------------------------------------------------------------------
DO $$
DECLARE
  started timestamptz := clock_timestamp();
  first_month date;
  m date;
  old_count bigint;
  new_count bigint;
BEGIN
  -- The global claim registry: compact, unpartitioned, the actual
  -- idempotency guarantee (phase-10 conversion_event_dedup pattern).
  CREATE TABLE billing.webhook_event_dedup (
    stripe_event_id text        PRIMARY KEY
  );
  GRANT SELECT, INSERT, DELETE ON billing.webhook_event_dedup TO sanvi_app;

  CREATE TABLE billing.webhook_events_p (
    stripe_event_id text        NOT NULL,
    type            text        NOT NULL,
    received_at     timestamptz NOT NULL DEFAULT now(),
    processed_at    timestamptz,
    api_version     text,
    payload         jsonb       NOT NULL,
    attempts        int         NOT NULL DEFAULT 0,
    error           text,
    created         timestamptz NOT NULL,
    PRIMARY KEY (stripe_event_id, received_at),
    CONSTRAINT billing_webhook_events_dedup_fkey
      FOREIGN KEY (stripe_event_id) REFERENCES billing.webhook_event_dedup (stripe_event_id)
  ) PARTITION BY RANGE (received_at);

  GRANT SELECT, INSERT, UPDATE ON billing.webhook_events_p TO sanvi_app;

  ALTER TABLE billing.webhook_events RENAME TO webhook_events_old;
  ALTER TABLE billing.webhook_events_p RENAME TO webhook_events;

  INSERT INTO platform.partitioned_tables (schema_name, table_name, key_column)
  VALUES ('billing', 'webhook_events', 'received_at');

  SELECT date_trunc('month', min(received_at))::date INTO first_month FROM billing.webhook_events_old;
  IF first_month IS NULL THEN
    first_month := date_trunc('month', current_date)::date;
  END IF;
  FOR m IN
    SELECT generate_series(first_month, date_trunc('month', current_date)::date,
                           interval '1 month')::date
  LOOP
    PERFORM platform.ensure_monthly_partitions(m);
  END LOOP;

  INSERT INTO billing.webhook_event_dedup (stripe_event_id)
  SELECT stripe_event_id FROM billing.webhook_events_old
  ON CONFLICT (stripe_event_id) DO NOTHING;

  INSERT INTO billing.webhook_events
    (stripe_event_id, type, received_at, processed_at, api_version,
     payload, attempts, error, created)
  SELECT stripe_event_id, type, received_at, processed_at, api_version,
         payload, attempts, error, created
  FROM billing.webhook_events_old
  ORDER BY received_at, stripe_event_id;

  SELECT count(*) INTO old_count FROM billing.webhook_events_old;
  SELECT count(*) INTO new_count FROM billing.webhook_events;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'billing.webhook_events conversion lost rows: % -> %', old_count, new_count;
  END IF;

  DROP TABLE billing.webhook_events_old;

  CREATE INDEX webhook_events_unprocessed_idx
    ON billing.webhook_events (received_at)
    WHERE processed_at IS NULL AND attempts < 5;
  ALTER INDEX billing.webhook_events_p_pkey RENAME TO webhook_events_pkey;

  RAISE NOTICE 'billing.webhook_events partition conversion: % rows in % ms',
    new_count, (clock_timestamp() - started);
END;
$$;

-- ---------------------------------------------------------------------------
-- payments.webhook_events (+ dedup sidecar)
-- ---------------------------------------------------------------------------
DO $$
DECLARE
  started timestamptz := clock_timestamp();
  first_month date;
  m date;
  old_count bigint;
  new_count bigint;
BEGIN
  CREATE TABLE payments.webhook_event_dedup (
    stripe_event_id text        PRIMARY KEY
  );
  GRANT SELECT, INSERT, DELETE ON payments.webhook_event_dedup TO sanvi_app;

  CREATE TABLE payments.webhook_events_p (
    stripe_event_id   text        NOT NULL,
    account_id        text,
    type              text        NOT NULL,
    payload           jsonb       NOT NULL,
    api_version       text,
    received_at       timestamptz NOT NULL DEFAULT now(),
    processed_at      timestamptz,
    attempts          int         NOT NULL DEFAULT 0,
    error             text,
    created           boolean     NOT NULL DEFAULT false,
    PRIMARY KEY (stripe_event_id, received_at),
    CONSTRAINT payments_webhook_events_dedup_fkey
      FOREIGN KEY (stripe_event_id) REFERENCES payments.webhook_event_dedup (stripe_event_id)
  ) PARTITION BY RANGE (received_at);

  GRANT SELECT, INSERT, UPDATE ON payments.webhook_events_p TO sanvi_app;

  ALTER TABLE payments.webhook_events RENAME TO webhook_events_old;
  ALTER TABLE payments.webhook_events_p RENAME TO webhook_events;

  INSERT INTO platform.partitioned_tables (schema_name, table_name, key_column)
  VALUES ('payments', 'webhook_events', 'received_at');

  SELECT date_trunc('month', min(received_at))::date INTO first_month FROM payments.webhook_events_old;
  IF first_month IS NULL THEN
    first_month := date_trunc('month', current_date)::date;
  END IF;
  FOR m IN
    SELECT generate_series(first_month, date_trunc('month', current_date)::date,
                           interval '1 month')::date
  LOOP
    PERFORM platform.ensure_monthly_partitions(m);
  END LOOP;

  INSERT INTO payments.webhook_event_dedup (stripe_event_id)
  SELECT stripe_event_id FROM payments.webhook_events_old
  ON CONFLICT (stripe_event_id) DO NOTHING;

  INSERT INTO payments.webhook_events
    (stripe_event_id, account_id, type, payload, api_version,
     received_at, processed_at, attempts, error, created)
  SELECT stripe_event_id, account_id, type, payload, api_version,
         received_at, processed_at, attempts, error, created
  FROM payments.webhook_events_old
  ORDER BY received_at, stripe_event_id;

  SELECT count(*) INTO old_count FROM payments.webhook_events_old;
  SELECT count(*) INTO new_count FROM payments.webhook_events;
  IF old_count <> new_count THEN
    RAISE EXCEPTION 'payments.webhook_events conversion lost rows: % -> %', old_count, new_count;
  END IF;

  DROP TABLE payments.webhook_events_old;

  CREATE INDEX webhook_events_unprocessed_idx
    ON payments.webhook_events (received_at)
    WHERE processed_at IS NULL AND attempts < 5;
  CREATE INDEX webhook_events_account_idx ON payments.webhook_events (account_id);
  ALTER INDEX payments.webhook_events_p_pkey RENAME TO webhook_events_pkey;

  RAISE NOTICE 'payments.webhook_events partition conversion: % rows in % ms',
    new_count, (clock_timestamp() - started);
END;
$$;
