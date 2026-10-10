# Runbook — partition maintenance & retention failure

**Alert:** `PartitionMaintenanceFailed` (see [advertising-alerts](./advertising-alerts.md)).

The automated database partition maintenance worker job failed to pre-create upcoming
monthly partitions for a registry parent — `advertising.conversion_events`,
`advertising.metrics_daily`, `audit.entries`, `platform.outbox`,
`billing.webhook_events` or `payments.webhook_events` — or aged advertising
partitions failed to be processed according to retention policies.

## Partitioning & retention architecture

TASK-028 (FR-1128) moved partition maintenance to one registry: every
partitioned parent lives in `platform.partitioned_tables`, and one
stored procedure creates and hardens children for all of them.

1. **Partitioned parents** (the registry, `SELECT * FROM platform.partitioned_tables`):
   - `advertising.conversion_events`: range on `occurred_at`.
   - `advertising.metrics_daily`: range on `date`.
   - `audit.entries`: range on `occurred_at`.
   - `platform.outbox`: range on `occurred_at`.
   - `billing.webhook_events` / `payments.webhook_events`: range on `received_at`
     (with a `webhook_event_dedup` sidecar owning global event-id claims).
2. **Maintenance stored procedure**:
   - `platform.ensure_monthly_partitions(target_date date)` (defined in
     `migrations/20261010000000_partition_registry.sql`).
   - Creates children named `<table>_YYYYMM` for the target month and
     `create_ahead_months` beyond it.
   - Copies the parent's `tenant_isolation` policy verbatim onto each child
     (audit/advertising parents), re-enables and forces RLS, and re-grants the
     parent's `sanvi_app` privileges per child — including DELETE where the
     parent carries it.
   - `advertising.ensure_monthly_partitions(target_date)` is a thin wrapper
     over the same function, kept for compatibility.
3. **Readiness probe** (the alert predicate, catalog-only — it fires even if
   the job itself is the thing that failed or was disabled):
   `platform.partition_readiness_breaches(reference_date)` returns one row per
   registry parent that has no next-month child, once the reference date is
   the 25th of the month or later. FR-1128: "alert when next month's partition
   does not exist by the 25th".
4. **Worker maintenance job**:
   - Job `"platform.partition_maintenance"` runs every
     `partition_maintenance_interval_secs = 86400` (24h) in
     `apps/worker/src/main.rs`.
   - Executes `SELECT platform.ensure_monthly_partitions(current_date)`,
     evaluates the readiness probe (an `error!` log carrying the
     `PartitionMaintenanceFailed` alert id on any breach), writes the
     `platform.partition_maintenance_log` heartbeat, then enforces the
     advertising retention rules via `advertising.enforce_retention` (unchanged,
     TASK-018).
5. **Statutory retention rules** (`privacy.retention_rules`):
   - `advertising_conversion_event`: 730 days (2 years), action `anonymise`.
   - `advertising_metric`: 1095 days (3 years), action `delete`.

## 1. Diagnose missing or failing partitions

One query covers every parent — this is the alert's probe:

```sql
SELECT * FROM platform.partition_readiness_breaches(current_date);
```

Non-empty from the 25th of the month on means the page is legitimate. On any
other day, list each parent's coverage directly:

```sql
SELECT r.schema_name, r.table_name, c.relname AS child,
       pg_get_expr(c.relpartbound, c.oid) AS bounds,
       c.relrowsecurity AS rls_enabled, c.relforcerowsecurity AS rls_forced
FROM platform.partitioned_tables r
LEFT JOIN pg_inherits i ON i.inhparent = format('%I.%I', r.schema_name, r.table_name)::regclass
LEFT JOIN pg_class c ON c.oid = i.inhrelid
WHERE r.enabled
ORDER BY 1, 2, 3;
```

Check the job's heartbeat (`platform.partition_maintenance_log`): a missing or
stale `ran_at` for today means the job has not run — the scheduler's leader
lock or the worker process is the suspect, not the function.

If an insert fails with `no partition of relation "..." found for row`,
the partition for the current or incoming month is missing on that parent.

## 2. Manual repair: pre-create partitions

Execute the partition maintenance function manually using `psql`:

```sql
-- Current month + one ahead, for every registry parent
SELECT platform.ensure_monthly_partitions(current_date);

-- Extend the horizon if the job will stay down
SELECT platform.ensure_monthly_partitions((current_date + interval '1 month')::date);
SELECT platform.ensure_monthly_partitions((current_date + interval '2 months')::date);
```

Verify that table permissions and RLS policies were applied (the function is
idempotent, so re-running it also repairs a child created out-of-band):

```sql
SELECT tablename, policyname, permissive, cmd
FROM pg_policies
WHERE policyname = 'tenant_isolation'
  AND (tablename ~ '^(conversion_events|metrics_daily)_\d{6}$'
       OR tablename ~ '^entries_\d{6}$')
ORDER BY tablename DESC;
```

## 3. Retention policy: Detach and drop aged partitions

When advertising partitions age beyond the statutory retention window defined in
`privacy.retention_rules` (e.g. `metrics_daily` older than 36 months / 1095 days):

1. **Verify candidate partitions for removal**:
   ```sql
   SELECT c.relname, pg_get_expr(c.relpartbound, c.oid) AS bounds
   FROM pg_class c
   JOIN pg_namespace n ON n.oid = c.relnamespace
   WHERE n.nspname = 'advertising'
     AND c.relname ~ '^metrics_daily_\d{6}$'
     AND c.relname < format('metrics_daily_%s', to_char(now() - interval '36 months', 'YYYYMM'))
   ORDER BY c.relname;
   ```
2. **Safely detach and archive/drop**:
   ```sql
   -- Detach partition first to avoid holding locks on parent table
   ALTER TABLE advertising.metrics_daily DETACH PARTITION advertising.metrics_daily_202301;

   -- Drop the detached table (or archive to cold storage per the archival runbook)
   DROP TABLE advertising.metrics_daily_202301;
   ```

   For the non-advertising parents, prefer the archival path
   ([archival](./archival.md)) over drops: `audit.entries` is append-only by
   trigger and `platform.outbox` / the webhook histories are the system's own
   operational record.

## 4. Verify

1. Test an insert and query against the new partition in staging/test database:
   ```sql
   EXPLAIN SELECT * FROM advertising.metrics_daily WHERE date = (current_date + interval '1 month')::date;
   ```
2. Confirm the readiness probe is quiet again:
   ```sql
   SELECT count(*) FROM platform.partition_readiness_breaches(current_date);  -- 0 before the 25th
   ```
3. Check worker logs to ensure the next `"platform.partition_maintenance"` tick
   runs cleanly without errors and `platform.partition_maintenance_log` shows
   today's heartbeat.
