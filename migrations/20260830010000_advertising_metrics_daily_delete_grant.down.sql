-- Restore the original grant (SELECT, INSERT, UPDATE only) for future
-- partitions and revoke DELETE from the parent and existing partitions.
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
    EXECUTE format('REVOKE DELETE ON advertising.%I FROM sanvi_app', partition.relname);
  END LOOP;
END;
$$;

REVOKE DELETE ON advertising.metrics_daily FROM sanvi_app;
