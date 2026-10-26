# Event recovery and worker management

PostgreSQL `platform.outbox` is the retained source and default delivery queue
for first-party events. The worker consumes published rows directly, using
PostgreSQL leases to coordinate each consumer group. Redis supports caches and
quotas; job coordination and event delivery state do not depend on Redis.
The optional Redis Streams library adapter still requires persistence and
`maxmemory-policy noeviction` if used.

Each required consumer reads retained, published outbox rows against its
PostgreSQL processed-event inbox and durable parked-event ledger. Polling runs
in bounded batches from the group's cursor in `platform.consumer_cursors`, so
settled history is never rescanned. An event whose outcome cannot be recorded
holds the cursor and is retried after the recovery interval (30 s), not at
poll speed. Dispatchers publish one transaction at a time under an advisory
lock; a stuck dispatcher transaction blocks publication, so alert on
`outbox_pending_gauge` growth. Notifications also read retained history; their existing
maximum-event-age policy decides whether old work is actionable. Explicit
tail-only consumers are unsuitable for required recoverable effects.

Recurring schedules and retry budgets live in `platform.scheduled_jobs`,
including `next_run_at`, `consecutive_failures`, `last_error` and
`last_finished_at`. `platform.worker_leases` exposes active ownership and expiry.
A crashed worker's unfinished work remains due; a replacement acquires the lease
after expiry. Repeated failures back off rather than resetting on restart.
See [ADR 0005](../adr/0005-postgres-jobs-and-outbox.md) for the initial migration
handover and rollback. Drain old workers before switching transport; their Redis
scheduler lock does not coordinate with PostgreSQL leases.

Do not manually archive, delete, or drop an outbox partition unless every
required consumer has processed or durably parked its events and the agreed
recovery window has elapsed. Preserve processed-event inbox records for at
least as long as any retained or archived event can be replayed. There is no
automatic stream/DLQ pruning in publication. Establish acknowledgment-aware
retention before introducing pruning; alert on capacity and oldest unprocessed
work in the meantime.

Consumer failures, including malformed raw messages, are durably retained in
`platform.consumer_dead_letters` before the source message is acknowledged.
When using the optional Redis adapter, its DLQ is a delivery/inspection copy. Triage with:

```sh
cargo run -p sanvi-cli -- dlq consumer-list --limit 20
cargo run -p sanvi-cli -- dlq consumer-replay --consumer access 873698342314721281
```

Replay is an audited transaction that removes the parked marker and requeues
the original event ID. Other consumers deduplicate the repeated delivery.
If the event's historical partition was manually removed, restore or recreate
that partition before replay; a failed replay transaction leaves the parked
record intact. Do not rewrite an event's occurrence time to bypass the issue.
Fix the underlying failure before replay. Malformed messages have no valid
event ID and require manual investigation rather than automatic replay.
Retain access restrictions and personal-data retention/erasure controls when
operating on raw failed payloads. Do not print payloads into incident logs.

The worker exposes a private management listener at `0.0.0.0:9091` by default
(`worker_management.host` and `worker_management.port`). It serves `/healthz`,
`/readyz`, and native Prometheus `/metrics`. Readiness requires PostgreSQL and
Redis probes; an unexpected background-task exit fails the worker and drains
the remaining tasks. Scrape `worker:9091/metrics` from the private network;
the supplied Compose file does not publish this port to the host. Configure
the collector to scrape this endpoint: native Prometheus collectors do not
automatically enter the OTLP pipeline.

Refund audit commands remain in their original refund rows until delivery;
the worker drains them every five seconds, committing each audit batch and
its delivery markers together. Failed delivery is retried without losing
the actor or the original request attribution.
