-- Down: retract the offboarding delete grants the same way they were
-- given — from the catalog, so the sets cannot drift.
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
    EXECUTE format('REVOKE DELETE ON %I.%I FROM sanvi_app',
                   target.table_schema, target.table_name);
  END LOOP;
  REVOKE DELETE ON tenancy.tenants FROM sanvi_app;
END $$;
