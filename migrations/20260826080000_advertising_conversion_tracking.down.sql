ALTER TABLE advertising.conversion_events DROP COLUMN value_source;

REVOKE SELECT, INSERT, UPDATE ON advertising.tracking_settings FROM sanvi_app;
DROP TABLE advertising.tracking_settings;
