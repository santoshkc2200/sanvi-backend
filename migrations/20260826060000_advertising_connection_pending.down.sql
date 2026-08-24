ALTER TABLE advertising.connections DROP COLUMN IF EXISTS pending_accounts;
ALTER TABLE advertising.connections DROP CONSTRAINT IF EXISTS connections_status_check;
ALTER TABLE advertising.connections ADD CONSTRAINT connections_status_check
  CHECK (status IN ('active', 'disconnected', 'expired'));
