-- Integration metadata only: no credentials, general ledger or stock quantities.
CREATE SCHEMA accounting;
GRANT USAGE ON SCHEMA accounting TO sanvi_app;

CREATE TABLE accounting.books (
    id bigint PRIMARY KEY CHECK (id > 0),
    tenant_id bigint NOT NULL CHECK (tenant_id > 0),
    legal_entity_key text NOT NULL CHECK (length(legal_entity_key) BETWEEN 1 AND 128 AND legal_entity_key = btrim(legal_entity_key)),
    revision bigint NOT NULL CHECK (revision >= 0),
    document jsonb NOT NULL CHECK (jsonb_typeof(document) = 'object'),
    UNIQUE (tenant_id, legal_entity_key),
    UNIQUE (tenant_id, id)
);
SELECT platform.enable_tenant_rls('accounting', 'books');

CREATE TABLE accounting.postings (
    id bigint PRIMARY KEY CHECK (id > 0),
    tenant_id bigint NOT NULL CHECK (tenant_id > 0),
    book_id bigint NOT NULL,
    connection_id bigint NOT NULL CHECK (connection_id > 0),
    source_event_id bigint NOT NULL CHECK (source_event_id > 0),
    fingerprint text NOT NULL CHECK (fingerprint ~ '^[0-9a-f]{64}$'),
    accounting_at timestamptz NOT NULL,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'submitted', 'delivered', 'failed', 'resolved_manually')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    failure_reason text CHECK (failure_reason IS NULL OR length(btrim(failure_reason)) BETWEEN 1 AND 1024),
    resolution_note text CHECK (resolution_note IS NULL OR length(btrim(resolution_note)) BETWEEN 1 AND 1024),
    resolved_by bigint CHECK (resolved_by IS NULL OR resolved_by > 0),
    status_changed_at timestamptz NOT NULL,
    external_record_id text CHECK (external_record_id IS NULL OR length(btrim(external_record_id)) BETWEEN 1 AND 256),
    FOREIGN KEY (tenant_id, book_id) REFERENCES accounting.books (tenant_id, id),
    UNIQUE (tenant_id, book_id, source_event_id),
    CHECK (
        (status = 'pending' AND external_record_id IS NULL AND resolution_note IS NULL AND resolved_by IS NULL) OR
        (status = 'submitted' AND external_record_id IS NULL AND resolution_note IS NULL AND resolved_by IS NULL) OR
        (status = 'delivered' AND external_record_id IS NOT NULL AND resolution_note IS NULL AND resolved_by IS NULL) OR
        (status = 'failed' AND failure_reason IS NOT NULL AND external_record_id IS NULL AND resolution_note IS NULL AND resolved_by IS NULL) OR
        (status = 'resolved_manually' AND resolution_note IS NOT NULL AND resolved_by IS NOT NULL)
    )
);
CREATE INDEX postings_open ON accounting.postings (tenant_id, book_id, status) WHERE status NOT IN ('delivered', 'resolved_manually');
SELECT platform.enable_tenant_rls('accounting', 'postings');

-- Terminal posting statuses are immutable.
CREATE FUNCTION accounting.postings_terminal_immutable() RETURNS trigger AS $$
BEGIN
  IF OLD.status IN ('delivered', 'resolved_manually') THEN
    RAISE EXCEPTION 'accounting posting in terminal status is immutable'
      USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END
$$ LANGUAGE plpgsql;

CREATE TRIGGER postings_terminal_immutable
  BEFORE UPDATE ON accounting.postings
  FOR EACH ROW EXECUTE FUNCTION accounting.postings_terminal_immutable();

-- Destination, fingerprint and source ownership cannot be changed by a delivery worker.
GRANT SELECT, INSERT ON accounting.books, accounting.postings TO sanvi_app;
GRANT UPDATE (revision, document) ON accounting.books TO sanvi_app;
GRANT UPDATE (status, attempts, failure_reason, resolution_note, resolved_by, status_changed_at, external_record_id) ON accounting.postings TO sanvi_app;
