-- 10.7 follow-up (TASK-016 review fix): `IngestConnectionMetrics::execute`
-- now clears a connection's existing `metrics_daily` rows for the
-- ingestion window before re-writing them — a restatement that drops a
-- previously reported campaign/day must delete that row, not leave it
-- stale. `sanvi_app` needs DELETE on `advertising.metrics_daily`: the
-- parent and every already-created monthly partition, since this schema
-- grants partition privileges individually (see `ensure_monthly_partitions`
-- below) rather than relying on the parent grant to cover them.

GRANT DELETE ON advertising.metrics_daily TO sanvi_app;

DO $$
DECLARE
  partition record;
BEGIN
  FOR partition IN
    SELECT c.relname
    FROM pg_inherits i
    JOIN pg_class c ON c.oid = i.inhrelid
    JOIN pg_class p ON p.oid = i.inhparent
    JOIN pg_namespace n ON n.oid = p.relnamespace
    WHERE n.nspname = 'advertising' AND p.relname = 'metrics_daily'
  LOOP
    EXECUTE format('GRANT DELETE ON advertising.%I TO sanvi_app', partition.relname);
  END LOOP;
END;
$$;

-- Re-declared with DELETE added to `metrics_daily` partitions' grant, so
-- every future partition stays consistent with the parent without a repeat
-- of this migration. Everything else is unchanged from the original.
CREATE OR REPLACE FUNCTION advertising.ensure_monthly_partitions(target_date date)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, advertising
AS $$
DECLARE
  month_start date := date_trunc('month', target_date)::date;
  month_end date := (month_start + interval '1 month')::date;
  suffix text := to_char(month_start, 'YYYYMM');
  conversion_child text := format('conversion_events_%s', suffix);
  metrics_child text := format('metrics_daily_%s', suffix);
BEGIN
  EXECUTE format(
    'CREATE TABLE IF NOT EXISTS advertising.%I PARTITION OF advertising.conversion_events FOR VALUES FROM (%L) TO (%L)',
    conversion_child, month_start, month_end
  );
  EXECUTE format('ALTER TABLE advertising.%I ENABLE ROW LEVEL SECURITY', conversion_child);
  EXECUTE format('ALTER TABLE advertising.%I FORCE ROW LEVEL SECURITY', conversion_child);
  EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON advertising.%I', conversion_child);
  EXECUTE format(
    'CREATE POLICY tenant_isolation ON advertising.%I USING (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::bigint) WITH CHECK (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::bigint)',
    conversion_child
  );
  EXECUTE format('GRANT SELECT, INSERT, UPDATE ON advertising.%I TO sanvi_app', conversion_child);

  EXECUTE format(
    'CREATE TABLE IF NOT EXISTS advertising.%I PARTITION OF advertising.metrics_daily FOR VALUES FROM (%L) TO (%L)',
    metrics_child, month_start, month_end
  );
  EXECUTE format('ALTER TABLE advertising.%I ENABLE ROW LEVEL SECURITY', metrics_child);
  EXECUTE format('ALTER TABLE advertising.%I FORCE ROW LEVEL SECURITY', metrics_child);
  EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON advertising.%I', metrics_child);
  EXECUTE format(
    'CREATE POLICY tenant_isolation ON advertising.%I USING (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::bigint) WITH CHECK (current_setting(''sanvi.platform_scope'', true) = ''on'' OR tenant_id = NULLIF(current_setting(''sanvi.tenant_id'', true), '''')::bigint)',
    metrics_child
  );
  EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON advertising.%I TO sanvi_app', metrics_child);
END;
$$;

REVOKE ALL ON FUNCTION advertising.ensure_monthly_partitions(date) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION advertising.ensure_monthly_partitions(date) TO sanvi_app;
