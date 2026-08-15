-- Reverts 20260101000000_platform_bootstrap.sql
REVOKE SELECT, INSERT, UPDATE ON platform.idempotency_keys FROM sanvi_app;
REVOKE SELECT, INSERT, UPDATE ON platform.outbox FROM sanvi_app;
REVOKE USAGE ON SCHEMA platform FROM sanvi_app;

DROP TABLE IF EXISTS platform.idempotency_keys;
DROP TABLE IF EXISTS platform.outbox;
DROP SCHEMA IF EXISTS platform;
