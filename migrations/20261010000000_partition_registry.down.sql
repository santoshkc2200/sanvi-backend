-- Down: restore phase 10's dedicated advertising function verbatim and
-- drop the generic registry. The unbounded-table conversions
-- (20261010000100) down-migrate first, so their registry rows are gone by
-- the time this runs.

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
  EXECUTE format('GRANT SELECT, INSERT, UPDATE ON advertising.%I TO sanvi_app', metrics_child);
END;
$$;

REVOKE ALL ON FUNCTION advertising.ensure_monthly_partitions(date) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION advertising.ensure_monthly_partitions(date) TO sanvi_app;

DROP FUNCTION IF EXISTS platform.partition_readiness_breaches(date);
DROP FUNCTION IF EXISTS platform.ensure_monthly_partitions(date);
DROP TABLE IF EXISTS platform.partition_maintenance_log;
DROP TABLE IF EXISTS platform.partitioned_tables;
