# Runbook — advertising partition maintenance & retention failure

**Alert:** `PartitionMaintenanceFailed` (see [advertising-alerts](./advertising-alerts.md)).

The automated database partition maintenance worker job failed to pre-create upcoming
monthly partitions for `advertising.conversion_events` or `advertising.metrics_daily`,
or aged partitions failed to be processed according to retention policies.

## Partitioning & Retention architecture

Sanvi partitions large advertising tables by range to ensure fast query performance,
bounded index sizes, and clean retention lifecycles:
1. **Partitioned parent tables**:
   - `advertising.conversion_events`: Partitioned by range on `occurred_at`.
   - `advertising.metrics_daily`: Partitioned by range on `date`.
2. **Maintenance stored procedure**:
   - `advertising.ensure_monthly_partitions(target_date date)` (defined in
     `migrations/20260826000000_advertising_core.sql` and updated in
     `migrations/20260830010000_advertising_metrics_daily_delete_grant.sql`).
   - Creates child tables named `conversion_events_YYYYMM` and `metrics_daily_YYYYMM`.
   - Enables and forces Row Level Security (RLS) with the standard `tenant_isolation` policy.
   - Grants `SELECT, INSERT, UPDATE, DELETE` permissions to `sanvi_app`.
3. **Worker maintenance job**:
   - Job `"advertising.partition_maintenance"` runs every
     `advertising_partition_maintenance_interval_secs = 86400` (24h) in
     `apps/worker/src/main.rs`.
   - Executes: `SELECT advertising.ensure_monthly_partitions((current_date + interval '1 month')::date)`.
4. **Statutory retention rules** (`privacy.retention_rules`):
   - `advertising_conversion_event`: 730 days (2 years), action `anonymise`.
   - `advertising_metric`: 1095 days (3 years), action `delete`.

## 1. Diagnose missing or failing partitions

List all active partitions and their bounds in PostgreSQL:

```sql
SELECT c.relname AS partition_name,
       pg_get_expr(c.relpartbound, c.oid) AS partition_bounds,
       c.relrowsecurity AS rls_enabled,
       c.relforcerowsecurity AS rls_forced
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'advertising'
  AND c.relname ~ '^(conversion_events|metrics_daily)_\d{6}$'
ORDER BY c.relname DESC;
```

Check if current and next month partitions are missing:

```sql
SELECT format('conversion_events_%s', to_char(current_date, 'YYYYMM')) AS current_month_expected,
       format('conversion_events_%s', to_char(current_date + interval '1 month', 'YYYYMM')) AS next_month_expected;
```

If an insert fails with `no partition of relation "conversion_events" found for row`,
the partition for the current or incoming month is missing.

## 2. Manual repair: Pre-create partitions

Execute the partition maintenance function manually using `psql`:

```sql
-- Pre-create current, next, and 2-month horizon partitions
SELECT advertising.ensure_monthly_partitions(current_date);
SELECT advertising.ensure_monthly_partitions((current_date + interval '1 month')::date);
SELECT advertising.ensure_monthly_partitions((current_date + interval '2 months')::date);
```

Verify that table permissions and RLS policies were applied:

```sql
-- Confirm RLS policy exists on the newly created partitions
SELECT tablename, policyname, permissive, cmd
FROM pg_policies
WHERE schemaname = 'advertising'
  AND tablename ~ '^(conversion_events|metrics_daily)_\d{6}$'
ORDER BY tablename DESC;
```

## 3. Retention policy: Detach and drop aged partitions

When partitions age beyond the statutory retention window defined in
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

   -- Drop the detached table (or archive to cold storage per compliance runbook)
   DROP TABLE advertising.metrics_daily_202301;
   ```

## 4. Verify

1. Test an insert and query against the new partition in staging/test database:
   ```sql
   EXPLAIN SELECT * FROM advertising.metrics_daily WHERE date = (current_date + interval '1 month')::date;
   ```
2. Check worker logs to ensure next `"advertising.partition_maintenance"` tick runs
   cleanly without errors.
