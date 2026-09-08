-- TASK-032: the media context brokers tenant uploads through
-- `hitox-media-service`. The bytes live there, not in this database, but the
-- data map has to disclose them: an uploaded image or video can depict a
-- person, and it is stored against an owner reference.
INSERT INTO privacy.retention_rules
  (data_class, sensitivity, basis, period, action, disclosed_in_notice, updated_by)
VALUES
  ('media_asset', 'standard', 'contract',
   make_interval(days => 1095), 'delete', true, '100')
ON CONFLICT (data_class) DO NOTHING;
