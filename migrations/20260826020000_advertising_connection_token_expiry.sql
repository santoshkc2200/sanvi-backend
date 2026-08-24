-- TASK-011: OAuth health must distinguish a live connection from one whose
-- access token has expired. Token material remains outside this table; only
-- the opaque vault reference from TASK-009 is persisted here.
ALTER TABLE advertising.connections
  ADD COLUMN token_expires_at timestamptz;
