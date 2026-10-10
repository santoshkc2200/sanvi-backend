-- TASK-028 (FR-1128): the single partition registry.
--
-- Phase 10 partitioned the advertising tables and hardcoded both parents
-- inside `advertising.ensure_monthly_partitions`. TASK-028 moves the
-- remaining unbounded tables (audit.entries, platform.outbox, the two
-- webhook event tables) to the same monthly-range shape, and that many
-- parents cannot live in one hardcoded function body. This migration gives
-- them all one home: `platform.partitioned_tables`, one generic
-- `platform.ensure_monthly_partitions`, and one readiness probe the daily
-- job and the alert rule both evaluate.
--
-- The generic function copies, per child:
--   * the parent's `tenant_isolation` policy verbatim (a tenant-keyed
--     parent must already carry the canonical policy — rls_matrix enforces
--     that — so copying it can never drift from the parent's shape), and
--   * the parent's sanvi_* grants, so a per-parent DELETE grant (the
--     advertising follow-up) survives without hand-maintained lists.
-- Children of platform-scoped parents (no tenant_id column) get grants
-- only, matching the webhook/outbox tables' existing shape.
--
-- `partition_readiness_breaches(reference_date)` is the alert predicate:
-- from the 25th of the month on, a registry parent without next month's
-- child is a breach. It reads the catalog only, so it fires whether the
-- maintenance job failed loudly or was disabled quietly — the probe does
-- not depend on the thing it watches.

CREATE TABLE platform.partitioned_tables (
  schema_name        text        NOT NULL,
  table_name         text        NOT NULL,
  -- Range key column; monthly bounds, phase-10 semantics (date literals
  -- cast per the server timezone, exactly as the advertising parents do).
  key_column         text        NOT NULL,
  -- How many months beyond the target month to pre-create.
  create_ahead_months int        NOT NULL DEFAULT 1,
  -- Per-child storage parameters (autovacuum tuning, TASK-028 step 5),
  -- applied as `WITH (...)` on every child this registry creates. Empty
  -- string keeps the cluster defaults.
  reloptions          text       NOT NULL DEFAULT '',
  enabled             boolean    NOT NULL DEFAULT true,
  created_at          timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (schema_name, table_name),
  CONSTRAINT partitioned_tables_strategy_check
    CHECK (create_ahead_months BETWEEN 0 AND 3)
);

-- Operational registry — no tenant rows, hence no tenant RLS.
GRANT SELECT ON platform.partitioned_tables TO sanvi_app;

-- One row per successful maintenance day: the job's heartbeat. Distinct
-- from the readiness probe (which reads the catalog), it answers "did the
-- job run at all".
CREATE TABLE platform.partition_maintenance_log (
  day      date        NOT NULL PRIMARY KEY,
  ran_at   timestamptz NOT NULL,
  ok       boolean     NOT NULL,
  detail   text
);

GRANT SELECT, INSERT, UPDATE ON platform.partition_maintenance_log TO sanvi_app;

-- Invoked by the leader-elected worker daily, and by migrations that
-- convert a table to this registry's shape. SECURITY DEFINER keeps DDL
-- privileges out of the NOBYPASSRLS application role; the fixed
-- search_path and registry/catalog-only identifiers avoid injection.
CREATE OR REPLACE FUNCTION platform.ensure_monthly_partitions(target_date date)
RETURNS integer
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog
AS $$
DECLARE
  reg        record;
  created    integer := 0;
  i          int;
  month_start date;
  month_end  date;
  suffix     text;
  child_name text;
  parent_oid oid;
  has_tenant boolean;
  policy_qual text;
  policy_check text;
  grant_row  record;
BEGIN
  FOR reg IN
    SELECT schema_name, table_name, key_column, create_ahead_months, reloptions
    FROM platform.partitioned_tables
    WHERE enabled
    ORDER BY schema_name, table_name
  LOOP
    parent_oid := to_regclass(format('%I.%I', reg.schema_name, reg.table_name));
    IF parent_oid IS NULL THEN
      -- A registry row may legitimately be deployed before its conversion
      -- migration lands (or after a down migration); skip loudly.
      RAISE WARNING 'partition registry row %.% has no table; skipping',
        reg.schema_name, reg.table_name;
      CONTINUE;
    END IF;

    SELECT EXISTS (
      SELECT 1 FROM pg_attribute a
      WHERE a.attrelid = parent_oid AND a.attname = 'tenant_id' AND a.attnum > 0
    ) INTO has_tenant;

    IF has_tenant THEN
      -- The canonical policy lives on the parent (rls_matrix pins it);
      -- the child gets a verbatim copy so the two can never drift.
      SELECT p.qual, p.with_check INTO policy_qual, policy_check
      FROM pg_policies p
      WHERE p.schemaname = reg.schema_name
        AND p.tablename = reg.table_name
        AND p.policyname = 'tenant_isolation';
      IF policy_qual IS NULL THEN
        RAISE EXCEPTION 'tenant-keyed parent %.% has no tenant_isolation policy to copy',
          reg.schema_name, reg.table_name;
      END IF;
    END IF;

    FOR i IN 0..CASE
          -- Pre-creating ahead of a *backfill* month is meaningless: ahead
          -- exists to pre-create the future, so the registry's horizon only
          -- applies from the current month on.
          WHEN date_trunc('month', target_date) >= date_trunc('month', current_date)
            THEN reg.create_ahead_months
          ELSE 0
        END LOOP
      month_start := date_trunc('month', target_date + make_interval(months => i))::date;
      month_end := (month_start + interval '1 month')::date;
      suffix := to_char(month_start, 'YYYYMM');
      child_name := format('%s_%s', reg.table_name, suffix);

      IF to_regclass(format('%I.%I', reg.schema_name, child_name)) IS NULL THEN
        EXECUTE format(
          'CREATE TABLE %I.%I PARTITION OF %I.%I FOR VALUES FROM (%L) TO (%L) %s',
          reg.schema_name, child_name, reg.schema_name, reg.table_name,
          month_start, month_end,
          CASE WHEN reg.reloptions = '' THEN ''
               ELSE format('WITH (%s)', reg.reloptions) END
        );
        created := created + 1;
      END IF;

      -- Idempotent hardening, same as phase 10's function: re-assert RLS
      -- and grants on every pass so a child created out-of-band (a manual
      -- restore, an operator minting a backfill month) converges too.
      IF has_tenant THEN
        EXECUTE format('ALTER TABLE %I.%I ENABLE ROW LEVEL SECURITY',
          reg.schema_name, child_name);
        EXECUTE format('ALTER TABLE %I.%I FORCE ROW LEVEL SECURITY',
          reg.schema_name, child_name);
        EXECUTE format('DROP POLICY IF EXISTS tenant_isolation ON %I.%I',
          reg.schema_name, child_name);
        EXECUTE format(
          'CREATE POLICY tenant_isolation ON %I.%I USING (%s) WITH CHECK (%s)',
          reg.schema_name, child_name, policy_qual, policy_check
        );
      END IF;

      FOR grant_row IN
        SELECT grantee, array_agg(DISTINCT privilege_type ORDER BY privilege_type) AS privs
        FROM information_schema.role_table_grants
        WHERE table_schema = reg.schema_name
          AND table_name = reg.table_name
          AND grantee LIKE 'sanvi%'
        GROUP BY grantee
      LOOP
        EXECUTE format('GRANT %s ON %I.%I TO %I',
          array_to_string(grant_row.privs, ', '),
          reg.schema_name, child_name, grant_row.grantee);
      END LOOP;
    END LOOP;
  END LOOP;
  RETURN created;
END;
$$;

-- The 25th-of-the-month alert predicate (FR-1128): from the 25th on, a
-- registry parent without next month's child is a breach. Catalog-only, so
-- it fires even when the maintenance job is the thing that failed.
CREATE OR REPLACE FUNCTION platform.partition_readiness_breaches(reference_date date)
RETURNS TABLE(schema_name text, table_name text, missing_child text)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = pg_catalog
AS $$
  SELECT r.schema_name,
         r.table_name,
         format('%s_%s', r.table_name,
                to_char(date_trunc('month', reference_date + interval '1 month'), 'YYYYMM'))
  FROM platform.partitioned_tables r
  WHERE r.enabled
    AND to_regclass(format('%I.%I', r.schema_name, r.table_name)) IS NOT NULL
    AND extract(day FROM reference_date)::int >= 25
    AND to_regclass(format('%I.%I', r.schema_name,
          format('%s_%s', r.table_name,
                 to_char(date_trunc('month', reference_date + interval '1 month'), 'YYYYMM')))) IS NULL
$$;

REVOKE ALL ON FUNCTION platform.ensure_monthly_partitions(date) FROM PUBLIC;
REVOKE ALL ON FUNCTION platform.partition_readiness_breaches(date) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION platform.ensure_monthly_partitions(date) TO sanvi_app;
GRANT EXECUTE ON FUNCTION platform.partition_readiness_breaches(date) TO sanvi_app;

-- The two phase-10 parents move into the registry. `advertising.
-- ensure_monthly_partitions` becomes a thin wrapper below, so its existing
-- callers (worker job, tests, seed-scale) keep working against one truth.
INSERT INTO platform.partitioned_tables (schema_name, table_name, key_column)
VALUES
  ('advertising', 'conversion_events', 'occurred_at'),
  ('advertising', 'metrics_daily', 'date');

CREATE OR REPLACE FUNCTION advertising.ensure_monthly_partitions(target_date date)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, advertising
AS $$
BEGIN
  PERFORM platform.ensure_monthly_partitions(target_date);
END;
$$;

-- Wrapper keeps the grants the original function carried (CREATE OR
-- REPLACE preserves the ACL); restate them for readability.
REVOKE ALL ON FUNCTION advertising.ensure_monthly_partitions(date) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION advertising.ensure_monthly_partitions(date) TO sanvi_app;
