# Runbook — retention class stalled (TASK-028, FR-1130)

**Alerts:** `RetentionClassStalled` — an executor-backed retention class has not recorded a run in `privacy.retention_runs` inside its 48-hour window (probe; see the [alerts registry](../../observability/alerts/rules.json)).

Phase 05 registered the retention rules; three executors enforce them:
`privacy.retention_sweep` (consent records, preference signals),
`platform.partition_maintenance` (the advertising classes) and
`platform.archival_sweep` (invoices, transaction records). The alert
means one of them stopped running — retention is silently not being
enforced, which is a compliance event, not a performance one.

## 1. Diagnose: which class, which executor

```sql
-- The alert's own probe, with names:
SELECT r.data_class
FROM privacy.retention_rules r
WHERE r.data_class IN ('consent_record', 'preference',
                       'advertising_conversion_event', 'advertising_metric',
                       'invoice', 'transaction_record')
  AND NOT EXISTS (
    SELECT 1 FROM privacy.retention_runs run
    WHERE run.data_class = r.data_class
      AND run.run_at > now() - interval '48 hours');

-- Full report from live data (rule, last run, rows affected, oldest
-- surviving row):
just retention-report
```

## 2. Find the executor

- **consent_record / preference** — the worker's `privacy.retention_sweep`
  job. Check the worker is running and leader-elected
  (`sanvi:scheduler:leader` in Redis); check `config.privacy.
  retention_dry_run` — dry-run passes execute rules but write nothing,
  and record no ledger rows.
- **advertising_conversion_event / advertising_metric** — the
  `platform.partition_maintenance` job. Its heartbeat is
  `platform.partition_maintenance_log`; see
  [partition maintenance failure](./advertising-partition-maintenance-failure.md).
- **invoice / transaction_record** — the `platform.archival_sweep` job.
  Requires `features.archival = true` **and** a configured envelope KEK;
  with either missing the sweep is a no-op and the classes go stale by
  design. See [archival](./archival.md).

## 3. Repair

1. Fix the executor (worker down → start it; flag off → turn it on after
   confirming the intent; KEK missing → configure `SANVI__PRIVACY__KEK`).
2. Run the executor once out-of-band if the schedule is long:
   the retention sweep runs with the worker; archival can be driven
   manually per the archival runbook.
3. Confirm the ledger moves:
   ```sql
   SELECT data_class, run_at, action, rows_affected, executor
   FROM privacy.retention_runs ORDER BY run_at DESC LIMIT 10;
   ```
4. Re-run `just retention-report` — it exits 0 only when every
   executor-backed class is inside its window.

## 4. Verify

- The alert clears on the next evaluation (the probe reads the ledger,
  not the job's self-report).
- TASK-029's evidence packs consume this ledger; a class that keeps
  stalling is a compliance finding, not just an ops one.
