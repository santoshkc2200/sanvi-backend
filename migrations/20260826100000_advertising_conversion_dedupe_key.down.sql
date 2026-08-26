ALTER TABLE advertising.conversion_events
  DROP CONSTRAINT conversion_events_tenant_id_dedupe_key_fkey;

-- Existing rows predating this migration already have one event id per
-- tenant. New rows may share an external event id across distinct names; use
-- their immutable key to preserve every row while restoring the old shape.
WITH duplicates AS (
  SELECT id, tenant_id, event_id,
         row_number() OVER (PARTITION BY tenant_id, event_id ORDER BY occurred_at, id) AS ordinal
  FROM advertising.conversion_events
)
UPDATE advertising.conversion_events AS event
SET event_id = event.event_id || ':' || event.dedupe_key
FROM duplicates
WHERE event.id = duplicates.id
  AND duplicates.ordinal > 1;

DELETE FROM advertising.conversion_event_dedup;

ALTER TABLE advertising.conversion_event_dedup
  DROP CONSTRAINT conversion_event_dedup_pkey,
  DROP COLUMN dedupe_key,
  ADD PRIMARY KEY (tenant_id, event_id);

INSERT INTO advertising.conversion_event_dedup (tenant_id, event_id, occurred_at)
SELECT tenant_id, event_id, min(occurred_at)
FROM advertising.conversion_events
GROUP BY tenant_id, event_id;

ALTER TABLE advertising.conversion_events
  DROP COLUMN dedupe_key,
  ADD CONSTRAINT conversion_events_tenant_id_event_id_fkey
  FOREIGN KEY (tenant_id, event_id)
  REFERENCES advertising.conversion_event_dedup(tenant_id, event_id);
