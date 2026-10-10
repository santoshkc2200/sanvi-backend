-- TASK-028 (FR-1131): the offboarding purge deletes the departing
-- tenant's rows across every context, as the app role (the CLI connects
-- as it; only the audit guard's lift needs the owner). Tenant
-- offboarding and retention are designed deleters now, so the app role
-- gets DELETE on the tenant-keyed tables — enumerated from the catalog
-- at migration time, in review like every table list.
--
-- `audit.entries` stays INSERT/SELECT-only: its rows are retained by
-- the purge (the chain is append-only by construction), and the
-- append-only trigger remains the second lock.
-- `tenancy.offboarding_requests` stays as granted in
-- 20261010000500 (SELECT, INSERT, UPDATE: the evidence row is never
-- deleted by the app role).

DO $$
DECLARE
  target record;
BEGIN
  FOR target IN
    SELECT c.table_schema, c.table_name
    FROM information_schema.columns c
    JOIN information_schema.tables t
      ON t.table_schema = c.table_schema AND t.table_name = c.table_name
     AND t.table_type = 'BASE TABLE'
    WHERE c.column_name = 'tenant_id'
      AND c.table_schema IN ('tenancy', 'identity', 'access', 'platform_admin',
                             'billing', 'payments', 'privacy', 'localization',
                             'theming', 'domains', 'advertising', 'media',
                             'notifications')
    GROUP BY 1, 2
  LOOP
    EXECUTE format('GRANT DELETE ON %I.%I TO sanvi_app',
                   target.table_schema, target.table_name);
  END LOOP;
  -- The anchor: its id *is* the tenant dimension.
  GRANT DELETE ON tenancy.tenants TO sanvi_app;
END $$;
