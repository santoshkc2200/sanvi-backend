# ADR 0005 — PostgreSQL jobs and transactional outbox delivery

- **Status:** accepted
- **Date:** 2026-10-26
- **Supersedes:** ADR 0001's Redis Streams delivery choice

## Decision

Use PostgreSQL as the durable source and execution coordinator for background
work. Context applications persist their state and domain events in one
transaction through `OutboxWriter`. They never publish directly to a broker.
Existing context-owned work tables (webhooks, exports, uploads and sagas) remain
queues owned by those contexts; periodic jobs drain them through their existing
application services.

The worker dispatches committed outbox rows by marking them published, then
consumer groups poll the retained rows. Each group uses a renewable PostgreSQL
lease, a durable processed-event inbox, and the existing durable dead-letter
ledger and audited replay command. A group processes one batch at a time;
different groups consume independently. Transactional handlers commit database
effects and their inbox row together. External effects require stable
idempotency keys and reconciliation. Delivery is at least once.

Recurring jobs persist their next due time, consecutive failure count, last
error and completion time in `platform.scheduled_jobs`. A PostgreSQL lease
coordinates the scheduler; completion advances the schedule only while that
lease is valid. Interrupted jobs stay due. Repeated failures suspend execution
for a bounded backoff before another probe; process restarts do not reset it.
Leases renew during long runs, expire after a crashed worker, and cancel local
work when renewal fails. No connection is held while external work runs.
Lease expiry cannot undo an external effect that already occurred, so recurring
jobs must also be idempotent.

`EventPublisher` is the broker boundary. The PostgreSQL adapter uses the
retained outbox row; Redis Streams remains a supported library adapter.
Future NATS or Kafka publishers must wait for durable broker acknowledgement
before the outbox marks publication complete. They must preserve event IDs,
context routing, consumer-group fan-out, inbox deduplication and replay.
Broker consumer adapters must reuse the handler contracts and durable inbox.
No NATS or Kafka infrastructure or dependency is introduced now.

## Operations and rollout

Apply `20261026000100_platform_postgres_jobs` before starting the new worker.
The schema expansion preserves all outbox, inbox and dead-letter data and
restores the dispatcher DELETE grant lost during outbox partition conversion. Existing
published history is recovered without copying events. Redis remains a cache
and quota dependency; handlers that touch it can still fail independently.

For this first transport transition, drain and stop every old worker before
starting PostgreSQL workers. Old binaries use a different scheduler lock and
do not participate in PostgreSQL consumer leases. Overlapping versions could
run scheduled or external effects concurrently. API writers may remain online;
committed outbox work waits safely during the worker handover. Subsequent
PostgreSQL worker replacements share coordination and durable schedules.

For application rollback, drain PostgreSQL workers before starting a compatible
previous worker image. Keep the added tables and migration history in place;
the previous Redis adapter recovers retained published outbox rows through its
existing PostgreSQL recovery path. Follow the deployment controller's image and
migration-history compatibility checks. Do not bypass those checks to deploy an
older image. The down migration is for a coordinated schema rollback only: it
removes coordination and schedule metadata, so schedule times and failure
budgets reset. It preserves event, inbox and dead-letter data.

## Consequences

No separate broker is needed for durable jobs or inter-context delivery. This
adds PostgreSQL polling and metadata writes; batch sizes and indexed context
selection bound each pass. A single consumer per group limits that group's
throughput. Retention must remain acknowledgement-aware: keep outbox history
and inbox records together until every required group has processed or durably
parked the events and the recovery window has elapsed. Add broker delivery when
measured fan-out or throughput needs justify it.
