-- Coordination and recurring-job state survive worker restarts and Redis loss.
CREATE TABLE platform.worker_leases (
    name text PRIMARY KEY,
    owner text NOT NULL,
    expires_at timestamptz NOT NULL,
    started_at timestamptz NOT NULL DEFAULT clock_timestamp()
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
CREATE INDEX outbox_context_delivery ON platform.outbox
    (split_part(aggregate, '.', 1), occurred_at, id) WHERE published_at IS NOT NULL;

-- Partition conversion replaced the parent without preserving the dispatcher's
-- DELETE grant. Moving exhausted events to the DLQ must remain possible.
GRANT DELETE ON platform.outbox TO sanvi_app;
