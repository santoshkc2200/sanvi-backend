-- 10.7 follow-up (TASK-016 review fix): a dedicated metrics-ingestion
-- cursor, separate from `last_synced_at` (owned by the OAuth refresh
-- lifecycle). Without this, a successful token refresh could make
-- `/ads/metrics/freshness` report a connection as current even when its
-- metrics ingestion sweep is failing or has never run — the two concerns
-- shared one timestamp by accident.
ALTER TABLE advertising.connections
  ADD COLUMN last_metrics_ingested_at timestamptz;
