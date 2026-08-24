-- TASK-011: durable terminal-refresh handling. Worker retries can restart,
-- but a revoked credential must still notify once and keep dependent jobs
-- paused until a confirmed refresh recovers the connection.
ALTER TABLE advertising.connections
  ADD COLUMN disconnected_notified_at timestamptz,
  ADD COLUMN dependent_jobs_paused_at timestamptz;
