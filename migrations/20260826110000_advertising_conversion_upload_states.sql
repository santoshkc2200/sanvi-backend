-- 10.6: conversion upload states is per-platform (platform_id -> upload_state).
-- Freshly captured events start with an empty map.
ALTER TABLE advertising.conversion_events
  RENAME COLUMN upload_state TO upload_states;

ALTER TABLE advertising.conversion_events
  ALTER COLUMN upload_states SET DEFAULT '{}'::jsonb;
