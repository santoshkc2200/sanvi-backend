DELETE FROM privacy.retention_rules WHERE data_class IN ('advertising_budget_cap', 'advertising_budget_alert');
ALTER TABLE advertising.campaign_changes DROP COLUMN IF EXISTS attribution, DROP COLUMN IF EXISTS metadata;
DROP TABLE IF EXISTS advertising.budget_alerts CASCADE;
DROP TABLE IF EXISTS advertising.budget_caps CASCADE;
