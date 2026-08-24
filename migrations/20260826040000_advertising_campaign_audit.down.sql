DELETE FROM privacy.retention_rules
WHERE data_class = 'advertising_campaign_change';

DROP TABLE IF EXISTS advertising.campaign_changes;

ALTER TABLE advertising.campaigns
  DROP COLUMN IF EXISTS remote_snapshot,
  DROP COLUMN IF EXISTS intent,
  DROP COLUMN IF EXISTS revision;
