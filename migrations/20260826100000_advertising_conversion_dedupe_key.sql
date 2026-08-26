-- Conversion delivery can happen before or after the asynchronous payment
-- webhook. Keep the vendor event id for downstream matching, but use an
-- immutable order/name identity for database idempotency.
ALTER TABLE advertising.conversion_events
  DROP CONSTRAINT conversion_events_tenant_id_event_id_fkey;

ALTER TABLE advertising.conversion_event_dedup
  ADD COLUMN dedupe_key text;

UPDATE advertising.conversion_event_dedup
SET dedupe_key = event_id
WHERE dedupe_key IS NULL;

ALTER TABLE advertising.conversion_event_dedup
  ALTER COLUMN dedupe_key SET NOT NULL;

ALTER TABLE advertising.conversion_events
  ADD COLUMN dedupe_key text;

UPDATE advertising.conversion_events AS event
SET dedupe_key = dedup.dedupe_key
FROM advertising.conversion_event_dedup AS dedup
WHERE dedup.tenant_id = event.tenant_id
  AND dedup.event_id = event.event_id
  AND event.dedupe_key IS NULL;

ALTER TABLE advertising.conversion_events
  ALTER COLUMN dedupe_key SET NOT NULL;

ALTER TABLE advertising.conversion_event_dedup
  DROP CONSTRAINT conversion_event_dedup_pkey,
  ADD PRIMARY KEY (tenant_id, dedupe_key);

ALTER TABLE advertising.conversion_events
  ADD CONSTRAINT conversion_events_tenant_id_dedupe_key_fkey
  FOREIGN KEY (tenant_id, dedupe_key)
  REFERENCES advertising.conversion_event_dedup(tenant_id, dedupe_key);
