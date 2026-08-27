ALTER TABLE advertising.conversion_events
  RENAME COLUMN upload_states TO upload_state;

ALTER TABLE advertising.conversion_events
  ALTER COLUMN upload_state SET DEFAULT '{}'::jsonb;
