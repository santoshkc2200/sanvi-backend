-- Coordination and recurring-job state survive worker restarts and Redis loss.
CREATE TABLE platform.worker_leases (
    name text PRIMARY KEY,
    owner text NOT NULL,
    expires_at timestamptz NOT NULL
);

CREATE TABLE platform.scheduled_jobs (
    scheduler_key text NOT NULL,
    name text NOT NULL,
    next_run_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    consecutive_failures integer NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error text,
    last_finished_at timestamptz,
    PRIMARY KEY (scheduler_key, name)
);

GRANT SELECT, INSERT, UPDATE ON platform.worker_leases, platform.scheduled_jobs TO sanvi_app;

-- Consumers read published rows in publication order. `published_at` cannot
-- serve: a dispatcher transaction can commit after a later one. Dispatchers
-- publish under one transaction-scoped advisory lock
-- (hashtextextended('platform.outbox.publication', 0)), so sequence order is
-- commit order and no row can later appear behind a consumer's cursor.
-- Rows published before this migration keep a NULL position and sort first.
-- Adding a nullable column without a default is a catalog-only change.
CREATE SEQUENCE platform.outbox_publication_seq;
GRANT USAGE, SELECT ON SEQUENCE platform.outbox_publication_seq TO sanvi_app;
ALTER TABLE platform.outbox ADD COLUMN published_seq bigint;

-- A trigger, not the dispatcher, assigns the position so operator replay and
-- every other writer keep it consistent with `published_at`.
CREATE FUNCTION platform.outbox_stamp_publication()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
  IF NEW.published_at IS NULL THEN
    NEW.published_seq := NULL;
  ELSIF TG_OP = 'INSERT' OR OLD.published_at IS NULL THEN
    NEW.published_seq := nextval('platform.outbox_publication_seq');
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER outbox_stamp_publication
  BEFORE INSERT OR UPDATE OF published_at ON platform.outbox
  FOR EACH ROW EXECUTE FUNCTION platform.outbox_stamp_publication();

-- Index each partition before the parent so the parent statement only attaches
-- them. API transactions insert into the current month, which sorts after
-- older months, so inserts wait only for the newest partitions' builds rather
-- than for the whole retained history. CREATE INDEX CONCURRENTLY cannot run on
-- a partitioned table or inside a migration transaction.
DO $$
DECLARE
  part record;
BEGIN
  FOR part IN
    SELECT c.relname
    FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
    WHERE i.inhparent = 'platform.outbox'::regclass
    ORDER BY c.relname
  LOOP
    EXECUTE format(
      'CREATE INDEX %I ON platform.%I
         (split_part(aggregate, ''.'', 1), COALESCE(published_seq, 0), id)
         WHERE published_at IS NOT NULL',
      part.relname || '_delivery_idx', part.relname);
  END LOOP;
END;
$$;

CREATE INDEX outbox_delivery_idx ON platform.outbox
    (split_part(aggregate, '.', 1), COALESCE(published_seq, 0), id)
    WHERE published_at IS NOT NULL;

-- Every position at or below a group's cursor is processed or durably parked
-- for that group, so delivery never rescans acknowledged history.
CREATE TABLE platform.consumer_cursors (
    consumer text NOT NULL,
    stream text NOT NULL,
    published_seq bigint NOT NULL,
    event_id bigint NOT NULL,
    PRIMARY KEY (consumer, stream)
);

GRANT SELECT, INSERT, UPDATE ON platform.consumer_cursors TO sanvi_app;

-- Partition conversion replaced the parent without preserving the dispatcher's
-- DELETE grant. Moving exhausted events to the DLQ must remain possible.
GRANT DELETE ON platform.outbox TO sanvi_app;
