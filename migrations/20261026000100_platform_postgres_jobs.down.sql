DROP INDEX IF EXISTS platform.outbox_context_delivery;
DROP TABLE IF EXISTS platform.scheduled_jobs;
DROP TABLE IF EXISTS platform.worker_leases;
REVOKE DELETE ON platform.outbox FROM sanvi_app;
