-- TASK-018: retention enforcement — age-out at the storage grain.
--
-- TASK-009 seeded privacy.retention_rules for the advertising classes
-- (conversion events 730d/anonymise, metrics 1095d/delete) but nothing
-- enforced them: partitions were only ever created. This closes the loop
-- with one SECURITY DEFINER function the daily partition-maintenance job
-- calls. Cutoffs are read from privacy.retention_rules by the caller so the
-- legal registry stays the single source of truth; the function itself only
-- knows dates and actions.
--
-- The registry's action decides what "past retention" means, and the two
-- actions are deliberately different *ends*, not a dress rehearsal for the
-- same drop:
--
--   delete    (advertising_metric) — the partition is detached and dropped.
--   anonymise (advertising_conversion_event) — the personal-data columns are
--             stripped (exactly the field set the erasure path
--             AdvertisingPrivacyProvider::erase anonymises) and the
--             partition is then RETAINED. The anonymised rows *are* the
--             retention state: the consent snapshot survives as proof under
--             statutory retention, the same RetainedItem story the erasure
--             path reports. Anonymising rows microseconds before dropping
--             the same partition would buy nothing — the retained form is
--             the point. Storage consequence: under the registered
--             `anonymise` action, aged conversion partitions accumulate.
--             That is the registry's decision to make, not this function's;
--             flipping the row to `delete` moves them to hard drop.
--
-- A NULL cutoff skips that class entirely — no rule, no write, no drop.
-- Every action is recorded in partition_retention_log, so the retention
-- story is auditable, not assumed.

CREATE TABLE advertising.partition_retention_log (
  partition_name text        NOT NULL,
  parent_name    text        NOT NULL,
  action         text        NOT NULL,
  recorded_at    timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (partition_name, parent_name, action)
);

-- Global operational audit rows — no tenant column, hence no tenant RLS.
GRANT SELECT, INSERT ON advertising.partition_retention_log TO sanvi_app;

CREATE OR REPLACE FUNCTION advertising.enforce_retention(
  conversion_cutoff     date,
  metrics_cutoff        date,
  anonymise_conversions boolean
) RETURNS integer
LANGUAGE plpgsql
SECURITY DEFINER
AS $$
DECLARE
  acted    integer := 0;
  partition record;
  month_end date;
  anonymised bigint := 0;
BEGIN
  FOR partition IN
    SELECT child.relname AS child_name, parent.relname AS parent_name
    FROM pg_inherits i
    JOIN pg_class    child  ON child.oid  = i.inhrelid
    JOIN pg_class    parent ON parent.oid = i.inhparent
    JOIN pg_namespace n    ON n.oid      = parent.relnamespace
    WHERE n.nspname = 'advertising'
      AND parent.relname IN ('conversion_events', 'metrics_daily')
  LOOP
    -- Partition names are `_YYYYMM` suffixes minted by
    -- ensure_monthly_partitions; anything unexpected is skipped loudly
    -- rather than acted on by guesswork.
    BEGIN
      month_end := (date_trunc(
        'month',
        to_date(right(partition.child_name, 6), 'YYYYMM')
      ) + interval '1 month - 1 day')::date;
    EXCEPTION WHEN others THEN
      RAISE WARNING 'unparseable advertising partition name %', partition.child_name;
      CONTINUE;
    END;

    IF partition.parent_name = 'conversion_events'
       AND conversion_cutoff IS NOT NULL
       AND month_end < conversion_cutoff
    THEN
      IF anonymise_conversions THEN
        -- The partition children carry FORCE ROW LEVEL SECURITY, which binds
        -- even this function's owner: the platform-scope flag is the
        -- audited escape platform-level maintenance runs under (same escape
        -- the persistence layer's unscoped transactions set). Transaction
        -- local, so it never leaks past the caller's statement.
        PERFORM set_config('sanvi.platform_scope', 'on', true);
        -- The same anonymisation the erasure path applies: subject link,
        -- identifier hashes and click ids go; the consent snapshot stays.
        -- Idempotent: rows already anonymised (by this sweep or an erasure
        -- request) are never rewritten, so later passes only scan.
        EXECUTE format(
          'UPDATE advertising.%I
             SET subject_key = ''{"kind": "contact"}''::jsonb,
                 hashed_ids  = ''{}''::jsonb,
                 click_ids   = ''{}''::jsonb
           WHERE subject_key->>''kind'' IS DISTINCT FROM ''contact''',
          partition.child_name
        );
        GET DIAGNOSTICS anonymised = ROW_COUNT;
        IF anonymised > 0 THEN
          INSERT INTO advertising.partition_retention_log
            (partition_name, parent_name, action)
          VALUES (partition.child_name, partition.parent_name, 'anonymise')
          ON CONFLICT (partition_name, parent_name, action) DO NOTHING;
          acted := acted + 1;
        END IF;
      ELSE
        EXECUTE format(
          'ALTER TABLE advertising.%I DETACH PARTITION advertising.%I',
          partition.parent_name, partition.child_name
        );
        EXECUTE format('DROP TABLE IF EXISTS advertising.%I', partition.child_name);
        INSERT INTO advertising.partition_retention_log
          (partition_name, parent_name, action)
        VALUES (partition.child_name, partition.parent_name, 'drop');
        acted := acted + 1;
      END IF;
    ELSIF partition.parent_name = 'metrics_daily'
       AND metrics_cutoff IS NOT NULL
       AND month_end < metrics_cutoff
    THEN
      EXECUTE format(
        'ALTER TABLE advertising.%I DETACH PARTITION advertising.%I',
        partition.parent_name, partition.child_name
      );
      EXECUTE format('DROP TABLE IF EXISTS advertising.%I', partition.child_name);
      INSERT INTO advertising.partition_retention_log
        (partition_name, parent_name, action)
      VALUES (partition.child_name, partition.parent_name, 'drop');
      acted := acted + 1;
    END IF;
  END LOOP;
  RETURN acted;
END $$;

-- Reserved for the maintenance job (the application role), like
-- ensure_monthly_partitions: SECURITY DEFINER functions are executable by
-- PUBLIC unless explicitly revoked.
REVOKE ALL ON FUNCTION advertising.enforce_retention(date, date, boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION advertising.enforce_retention(date, date, boolean) TO sanvi_app;
