-- TASK-028 (FR-1129): the archival registry.
--
-- `platform.archives` records every range the mover moved to cold storage:
-- what table, what time range, whose rows (NULL tenant_id = the
-- platform-scope rows), how many, and the checksum that proves the restore
-- brought back identical rows. A range is per-tenant by construction — a
-- tenant's offboarding purge can delete its archives without touching a
-- neighbour's, which a whole-range archive could never promise.
--
-- Statuses: 'archived' (active — a read over the range must say so, never
-- return a silent partial), 'restored' (back in the table; reads are
-- normal again), 'purged' (the cold object was destroyed as part of a
-- tenant offboarding; the row stays as the evidence that it was).

CREATE TABLE platform.archives (
  id          bigint        PRIMARY KEY,
  schema_name text          NOT NULL,
  table_name  text          NOT NULL,
  -- The range key column the mover ordered and sliced on.
  key_column  text          NOT NULL,
  -- NULL = platform-scope rows (rows with no tenant, e.g. audit entries
  -- written by platform actors).
  tenant_id   bigint,
  range_start timestamptz   NOT NULL,
  range_end   timestamptz   NOT NULL,
  row_count   bigint        NOT NULL,
  -- sha256 over the canonical serialized row set (key order), hex.
  checksum    text          NOT NULL,
  storage_path text         NOT NULL,
  bytes       bigint        NOT NULL,
  status      text          NOT NULL DEFAULT 'archived'
    CHECK (status IN ('archived', 'restored', 'purged')),
  created_at  timestamptz   NOT NULL DEFAULT now(),
  restored_at timestamptz,
  purged_at   timestamptz,
  CONSTRAINT archives_range_check CHECK (range_end > range_start)
);

-- One active archive per (table, tenant scope, range): re-archiving the
-- same range must update/skip, never fork. COALESCE folds the NULL
-- platform-scope rows into their own scope.
CREATE UNIQUE INDEX archives_active_unique
  ON platform.archives (schema_name, table_name, COALESCE(tenant_id, 0),
                        range_start, range_end)
  WHERE status = 'archived';

CREATE INDEX archives_table_status_idx
  ON platform.archives (schema_name, table_name, status, range_start);

-- Global operational registry — no tenant rows, hence no tenant RLS.
GRANT SELECT, INSERT, UPDATE ON platform.archives TO sanvi_app;
