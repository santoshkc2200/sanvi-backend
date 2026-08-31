DROP INDEX IF EXISTS advertising.idx_advertising_metrics_daily_tenant_platform_date;
REVOKE SELECT, INSERT, UPDATE, DELETE ON advertising.metrics_unattributed_revenue_daily FROM sanvi_app;
DROP TABLE IF EXISTS advertising.metrics_unattributed_revenue_daily;
