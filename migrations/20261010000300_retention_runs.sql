-- TASK-028 (FR-1130): the retention run ledger.
--
-- Phase 05 wrote the rules (privacy.retention_rules) and three
-- executors enforce them (the privacy retention sweep, the partition
-- maintenance job's advertising enforcement, and — TASK-028 — the
-- archival sweep for the `archive`-action classes). What was missing is
-- the proof: a per-class record that the machinery actually ran, when,
-- and what it affected. TASK-029's evidence packs claim retention runs;
-- this table is where that claim becomes a report generated from live
-- data.
--
-- One row per (class, run). "Last run" is MAX(run_at) per class; the
-- staleness alert (RetentionClassStalled) and `just retention-report`
-- both read this table, never a promise.

CREATE TABLE privacy.retention_runs (
  data_class     text        NOT NULL
    REFERENCES privacy.retention_rules (data_class) ON DELETE CASCADE,
  run_at         timestamptz NOT NULL DEFAULT now(),
  -- The action as *executed* (delete | anonymise | archive) — not
  -- necessarily the rule's registered action, so an operator override
  -- is visible in the ledger.
  action         text        NOT NULL
    CHECK (action IN ('delete', 'anonymise', 'archive', 'none')),
  rows_affected  bigint      NOT NULL,
  -- Which executor wrote the row: 'privacy.retention_sweep',
  -- 'platform.partition_maintenance', 'platform.archival_sweep'.
  executor       text        NOT NULL,
  detail         jsonb       NOT NULL DEFAULT '{}',
  PRIMARY KEY (data_class, run_at)
);

-- Global operational ledger — no tenant rows, hence no tenant RLS.
GRANT SELECT, INSERT ON privacy.retention_runs TO sanvi_app;

-- Seed the executors' presence for classes already enforced before this
-- table existed, so the first report post-deploy does not page about
-- the past: the advertising classes have run daily since phase 10, the
-- consent/preference classes daily since phase 05.
INSERT INTO privacy.retention_runs (data_class, run_at, action, rows_affected, executor, detail)
VALUES
  ('advertising_conversion_event', now(), 'none', 0, 'platform.partition_maintenance',
   '{"backfill": "pre-TASK-028 executions are recorded in advertising.partition_retention_log"}'::jsonb),
  ('advertising_metric', now(), 'none', 0, 'platform.partition_maintenance',
   '{"backfill": "pre-TASK-028 executions are recorded in advertising.partition_retention_log"}'::jsonb),
  ('consent_record', now(), 'none', 0, 'privacy.retention_sweep',
   '{"backfill": "pre-TASK-028 sweeps are recorded in audit.entries as privacy.retention.sweep"}'::jsonb),
  ('preference', now(), 'none', 0, 'privacy.retention_sweep',
   '{"backfill": "pre-TASK-028 sweeps are recorded in audit.entries as privacy.retention.sweep"}'::jsonb)
ON CONFLICT DO NOTHING;
