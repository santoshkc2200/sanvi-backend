-- TASK-011: a callback that has exchanged a code for a refresh token but
-- has not yet had an account chosen is `pending`, not `active` — it can sync
-- nothing and must not be treated as a live connection by any other slice.
-- `pending_accounts` caches the platform's account list for the picker so
-- the exchanged code (single-use, already spent) never has to be replayed.
ALTER TABLE advertising.connections DROP CONSTRAINT connections_status_check;
ALTER TABLE advertising.connections ADD CONSTRAINT connections_status_check
  CHECK (status IN ('pending', 'active', 'disconnected', 'expired'));

ALTER TABLE advertising.connections ADD COLUMN pending_accounts jsonb;
