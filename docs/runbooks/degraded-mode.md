# Read-only recovery and degraded-mode procedure

Owner and decision maker: platform incident commander. On-call staffing and
external status-page publishing are parked in `docs/release/needs-humans.md`.

1. Record the incident/restore start time. Tell affected tenants that writes are
   paused for recovery, the affected window, and when the next update is due.
   Use the available support channel; the backend does not publish a status page.
2. At the edge, route interactive writes to maintenance responses and retain
   only explicitly approved safe reads. Stop worker/scheduler intake and drain
   in-flight batches before recovery. Pause external callback delivery with its
   durable retry mechanism; never acknowledge events that were not persisted.
3. Enable `SANVI__SERVER__DEGRADED_MODE__ENABLED=true` on API startup. This is
   the configuration backing `platform.degraded_mode`: it stamps active
   dependency reasons; it is **not** a write lock or recovery detector.
4. Restore following `dr-restore.md`. While read-only, use fresh connections
   with `PGOPTIONS='-c default_transaction_read_only=on'`. Verify
   `SHOW transaction_read_only` is `on`, `SELECT 1` succeeds, and a write is
   rejected with SQLSTATE `25006`. An unrelated permission error is not proof.
   Database-backed writes otherwise become opaque errors; do not advertise
   ordinary service during this period. `/health` remains liveness-only and
   `/ready` uses `SELECT 1`, so a green readiness response proves connectivity,
   **not write availability**. Keep maintenance routing until a write probe passes.
5. After promotion, verify `pg_is_in_recovery()` is false, a fresh connection
   reports read-write, and a controlled write transaction succeeds. Verify the
   audit chain, outbox/idempotency state and payments before resuming delivery.
6. Restart workers, smoke-test tenant reads/writes, remove maintenance routing,
   restore the normal degraded-header flag posture, and send the tenant update.
   Record end time, local RPO/RTO and the decision maker.

## Local rehearsal

`scripts/db/restore.sh --session <sid> --rehearse` runs
`scripts/db/degraded-mode.sh <restored-drill-container>` before publishing the
report. It uses the actual `sanvi_app` role, tests read-only rejection with
`25006`, then proves a fresh normal connection accepts a harmless write
command (`UPDATE ... WHERE false`). No tenant row changes. Read-only mode is
connection-local: this demonstrates the database procedure without affecting
the dev stack. Edge maintenance, tenant messages and staffing remain parked.

The dated execution evidence is the `checks.degraded_mode` field in the committed
full/PITR rehearsal reports under `benchmarks/dr/`.
