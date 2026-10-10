# Partition-conversion rehearsal — TASK-028 step 2 (FR-1128)

**Lab, not production.** 2026-10-10, the shared test Postgres 18
container (tmpfs `PGDATA`, warm binary). The numbers below size the
*maintenance window conversation*; production scheduling re-measures on
the scale seed with the same procedure.

## Seed

| Table | Rows converted |
|---|---:|
| `audit.entries` | 200,000 |
| `platform.outbox` | 100,000 |
| `billing.webhook_events` | 50,000 |
| `payments.webhook_events` | 50,000 |

Rows spread over ~5 months (one partition minted per month by the
registry during the conversion), narrow payloads.

## Measured

| Operation | Wall time |
|---|---|
| Apply the TASK-028 migration set (`20261010000100` → `20261010000700`, the four conversions + registry + archival + grants + tuning) | **0.62 s** |
| Revert the set (count-verified copies back to unpartitioned shapes) | **1.16 s** |

Every conversion is one transaction: create the partitioned shape,
verified copy (count-checked, refuses on mismatch), atomic rename swap,
drop the old table, recreate indexes/RLS/triggers/grants. The migration
emits a `NOTICE` with its per-table row count and duration.

## What this means for the window

- On lab hardware the copy phase is ~10⁵ rows/second/table, so the
  driver is **production row counts × row width**, not the procedure.
- The swap takes an `ACCESS EXCLUSIVE` lock on each table for the copy
  duration: on a hot tenant's live history that is the outage. The
  rehearsal procedure for production:

  1. `just seed-scale`, note the hot tenant's `audit.entries` row count.
  2. Apply on a restored snapshot first (same procedure, real width) —
     record the duration here.
  3. Schedule the live window from that number ×2, per the runbook.

- Reversibility was rehearsed: the down migrations rebuilt the original
  shapes and row counts verified (200k/100k/50k/50k round-tripped).

## Production rehearsal log

| Date | Environment | audit rows | Duration (up) | Duration (down) | Notes |
|---|---|---:|---|---|---|
| 2026-10-10 | test container, synthetic | 200,000 | 0.62 s | 1.16 s | this document's seed |
| | scale seed (`just seed-scale`) | | | | *(record the scale rehearsal here before scheduling production)* |
