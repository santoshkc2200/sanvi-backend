-- Down: restore default autovacuum on everything tuned, clear the
-- registry's reloptions, and recreate the two pruned indexes.
UPDATE platform.partitioned_tables SET reloptions = ''
WHERE (schema_name, table_name) IN
  (('audit', 'entries'), ('platform', 'outbox'),
   ('billing', 'webhook_events'), ('payments', 'webhook_events'),
   ('advertising', 'conversion_events'));

DO $$
DECLARE
  child record;
BEGIN
  FOR child IN
    SELECT c.relname, n.nspname
    FROM pg_inherits i
    JOIN pg_class p ON p.oid = i.inhparent
    JOIN pg_namespace n ON n.oid = p.relnamespace
    JOIN pg_class c ON c.oid = i.inhrelid
    WHERE (n.nspname, p.relname) IN
      (('audit', 'entries'), ('platform', 'outbox'),
       ('billing', 'webhook_events'), ('payments', 'webhook_events'),
       ('advertising', 'conversion_events'))
      AND c.relname ~ '_[0-9]{6}$'
  LOOP
    EXECUTE format('ALTER TABLE %I.%I RESET (autovacuum_vacuum_scale_factor, autovacuum_analyze_scale_factor, autovacuum_vacuum_insert_scale_factor)',
                   child.nspname, child.relname);
  END LOOP;
END $$;

ALTER TABLE billing.invoices RESET (autovacuum_vacuum_scale_factor, autovacuum_vacuum_threshold);
ALTER TABLE payments.payments RESET (autovacuum_vacuum_scale_factor, autovacuum_vacuum_threshold);
ALTER TABLE payments.checkouts RESET (autovacuum_vacuum_scale_factor, autovacuum_vacuum_threshold);

DO $$
DECLARE
  target record;
BEGIN
  FOR target IN
    SELECT c.table_schema, c.table_name
    FROM information_schema.columns c
    WHERE c.column_name = 'tenant_id'
      AND (c.table_schema, c.table_name) IN (
        ('billing', 'subscriptions'), ('payments', 'tax_settings'),
        ('payments', 'provider_connections'), ('advertising', 'connections'),
        ('advertising', 'tracking_settings'), ('advertising', 'tenant_secret_keys'),
        ('advertising', 'guardrail_suspensions'))
  LOOP
    EXECUTE format('ALTER TABLE %I.%I RESET (autovacuum_vacuum_threshold)',
                   target.table_schema, target.table_name);
  END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS payments_tenant_idx ON payments.payments (tenant_id);
CREATE INDEX IF NOT EXISTS disputes_created_idx
  ON payments.disputes (tenant_id, created_at DESC, id DESC);
