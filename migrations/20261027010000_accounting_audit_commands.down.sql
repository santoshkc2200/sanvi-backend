-- Deliver or export pending rows (delivered_at IS NULL) to the audit chain
-- before rolling back: dropping the table discards their only copy.
DROP TABLE accounting.audit_commands;
DROP FUNCTION accounting.audit_commands_immutable();
