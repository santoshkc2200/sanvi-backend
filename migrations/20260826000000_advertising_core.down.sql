-- TASK-009 rollback rehearsal. Operators normally roll back by disabling
-- `features.advertising`; the empty schema may remain safely deployed.
DELETE FROM privacy.retention_rules
WHERE data_class IN ('advertising_conversion_event', 'advertising_metric')
  AND updated_by = '100';
DROP SCHEMA IF EXISTS advertising CASCADE;
