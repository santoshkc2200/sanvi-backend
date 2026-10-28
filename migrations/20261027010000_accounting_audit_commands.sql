CREATE TABLE accounting.audit_commands (
    id bigint PRIMARY KEY CHECK (id > 0),
    tenant_id bigint NOT NULL CHECK (tenant_id > 0),
    book_id bigint NOT NULL,
    entry jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    delivered_at timestamptz,
    FOREIGN KEY (tenant_id, book_id) REFERENCES accounting.books (tenant_id, id)
);
CREATE INDEX audit_commands_pending ON accounting.audit_commands (created_at, id) WHERE delivered_at IS NULL;
SELECT platform.enable_tenant_rls('accounting', 'audit_commands');
GRANT SELECT, INSERT ON accounting.audit_commands TO sanvi_app;
GRANT UPDATE (delivered_at) ON accounting.audit_commands TO sanvi_app;

CREATE FUNCTION accounting.audit_commands_immutable() RETURNS trigger AS $$
BEGIN
  IF OLD.delivered_at IS NOT NULL THEN
    RAISE EXCEPTION 'delivered accounting audit command is immutable'
      USING ERRCODE = 'check_violation';
  END IF;
  IF NEW.id <> OLD.id OR NEW.tenant_id <> OLD.tenant_id OR NEW.book_id <> OLD.book_id OR NEW.entry <> OLD.entry OR NEW.created_at <> OLD.created_at THEN
    RAISE EXCEPTION 'accounting audit command fields other than delivered_at are immutable'
      USING ERRCODE = 'check_violation';
  END IF;
  RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER audit_commands_immutable
  BEFORE UPDATE ON accounting.audit_commands
  FOR EACH ROW EXECUTE FUNCTION accounting.audit_commands_immutable();
