-- TASK-028 (FR-1131): the offboarding record.
--
-- One row per export-then-purge request for a departing tenant. The API
-- creates it (idempotent by key); the CLI executor advances it through
-- the states and writes the export evidence and the purge report back.
-- The purge is deliberately not reversible, so the record keeps every
-- state change timestamp and the final purge report — removed vs
-- lawfully retained, with the legal basis and retention end date of
-- everything kept.

CREATE TABLE tenancy.offboarding_requests (
  id              bigint        PRIMARY KEY,
  -- Deliberately NOT a foreign key: this row is the evidence that the
  -- purge happened, so it must survive the tenant row it describes
  -- (the purge deletes the tenant; the report stays).
  tenant_id       bigint        NOT NULL,
  -- Same key + tenant returns the existing request: starting an
  -- offboarding twice must never fork the record.
  idempotency_key text          NOT NULL,
  status          text          NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending', 'exporting', 'exported',
                      'purging', 'purged', 'failed')),
  -- The operator who requested it through the API (NULL when the CLI
  -- drove the whole flow, e.g. a drill).
  requested_by    bigint,
  -- The verified export: path on the artifact volume + whole-archive
  -- checksum. The purge step is unreachable until these are set.
  export_path     text,
  export_checksum text,
  -- What was removed per context, what was lawfully retained and why,
  -- with each retained class's legal basis and retention end date.
  purge_report    jsonb,
  error           text,
  created_at      timestamptz   NOT NULL DEFAULT now(),
  updated_at      timestamptz   NOT NULL DEFAULT now(),
  completed_at    timestamptz,
  UNIQUE (tenant_id, idempotency_key)
);

SELECT platform.enable_tenant_rls('tenancy', 'offboarding_requests');

CREATE INDEX offboarding_requests_tenant_idx
  ON tenancy.offboarding_requests (tenant_id, created_at DESC);

GRANT SELECT, INSERT, UPDATE ON tenancy.offboarding_requests TO sanvi_app;
