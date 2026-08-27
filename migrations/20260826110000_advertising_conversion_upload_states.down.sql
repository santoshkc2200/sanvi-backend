ALTER TABLE advertising.conversion_events
  RENAME COLUMN upload_states TO upload_state;

UPDATE advertising.conversion_events
  SET upload_state = '{"status":"pending"}'::jsonb;

