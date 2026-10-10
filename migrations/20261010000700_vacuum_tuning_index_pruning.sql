-- TASK-028 (FR-1130, step 5): vacuum tuning per table size + index
-- pruning.
--
-- The defaults are wrong at both ends. The hot, append-mostly tables
-- (audit entries, the outbox, the webhook histories, conversion
-- events) accumulate tens of dead tuples between default autovacuum
-- passes — the explain review's "map filled" window is exactly that
-- lag — while the tiny per-tenant lookup tables (a handful of rows
-- each, hundreds of them) get vacuumed constantly for nothing.
--
-- Per-parent values are recorded in the partition registry's
-- `reloptions`, so every child this registry creates is born tuned
-- (Postgres allows storage parameters only on storage-bearing leaf
-- tables, never on the partitioned parent itself); the same values are
-- applied here to the existing children. The lookup tables get a high
-- threshold so autovacuum visits them rarely (they still get ANALYZE on
-- first insert).

-- ---------------------------------------------------------------------------
-- Hot parents (and their children, born tuned via the registry)
-- ---------------------------------------------------------------------------
UPDATE platform.partitioned_tables
SET reloptions = 'autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.01, autovacuum_vacuum_insert_scale_factor = 0.02'
WHERE (schema_name, table_name) IN
  (('audit', 'entries'), ('platform', 'outbox'),
   ('billing', 'webhook_events'), ('payments', 'webhook_events'),
   ('advertising', 'conversion_events'));

-- Existing children of those parents (monthly names only — minted by the
-- registry's own function).
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
    EXECUTE format('ALTER TABLE %I.%I SET (
      autovacuum_vacuum_scale_factor = 0.02, autovacuum_analyze_scale_factor = 0.01,
      autovacuum_vacuum_insert_scale_factor = 0.02)', child.nspname, child.relname);
  END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- Tenant money tables (update-mostly): vacuum on a low fixed threshold so
-- status transitions do not wait for a percentage of a growing table.
-- ---------------------------------------------------------------------------
ALTER TABLE billing.invoices SET (
  autovacuum_vacuum_scale_factor = 0.05, autovacuum_vacuum_threshold = 1000);
ALTER TABLE payments.payments SET (
  autovacuum_vacuum_scale_factor = 0.05, autovacuum_vacuum_threshold = 1000);
ALTER TABLE payments.checkouts SET (
  autovacuum_vacuum_scale_factor = 0.05, autovacuum_vacuum_threshold = 1000);

-- ---------------------------------------------------------------------------
-- Tiny per-tenant lookup tables: a threshold so high autovacuum leaves
-- them alone (they never churn); ANALYZE still runs on first inserts.
-- ---------------------------------------------------------------------------
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
      AND (c.table_schema, c.table_name) IN (
        ('billing', 'subscriptions'), ('payments', 'tax_settings'),
        ('payments', 'provider_connections'), ('advertising', 'connections'),
        ('advertising', 'tracking_settings'), ('advertising', 'tenant_secret_keys'),
        ('advertising', 'guardrail_suspensions'))
  LOOP
    EXECUTE format(
      'ALTER TABLE %I.%I SET (autovacuum_vacuum_threshold = 100000)',
      target.table_schema, target.table_name);
  END LOOP;
END $$;

-- ---------------------------------------------------------------------------
-- Index pruning: two redundancies measured against the plan inventory
-- (benchmarks/explain-2026-10-06.md + the pg_indexes audit of 2026-10-10):
--
--   payments.payments_tenant_idx (tenant_id) — the prefix of
--     payments_tenant_created_id_idx (tenant_id, created_at DESC, id DESC)
--     (and of payments_tenant_currency_idx); no plan reads it that the
--     composite cannot serve.
--   payments.disputes_created_idx (tenant_id, created_at DESC, id DESC) —
--     a byte-for-byte duplicate of disputes_tenant_idx after the 20261006
--     realignment realigned the latter onto the same shape.
--     explain_assertions pins disputes_tenant_idx; the duplicate doubles
--     every dispute write's index maintenance for nothing.
DROP INDEX IF EXISTS payments.payments_tenant_idx;
DROP INDEX IF EXISTS payments.disputes_created_idx;
