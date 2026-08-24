ALTER TABLE advertising.connections
  DROP COLUMN IF EXISTS dependent_jobs_paused_at,
  DROP COLUMN IF EXISTS disconnected_notified_at;
