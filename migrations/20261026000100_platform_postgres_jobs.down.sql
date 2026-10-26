DROP TABLE IF EXISTS platform.consumer_cursors;
DROP INDEX IF EXISTS platform.outbox_delivery_idx;
DROP TRIGGER IF EXISTS outbox_stamp_publication ON platform.outbox;
DROP FUNCTION IF EXISTS platform.outbox_stamp_publication();
ALTER TABLE platform.outbox DROP COLUMN IF EXISTS published_seq;
DROP SEQUENCE IF EXISTS platform.outbox_publication_seq;
DROP TABLE IF EXISTS platform.scheduled_jobs;
DROP TABLE IF EXISTS platform.worker_leases;
REVOKE DELETE ON platform.outbox FROM sanvi_app;
