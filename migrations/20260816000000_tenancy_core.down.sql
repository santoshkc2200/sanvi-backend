-- Revert phase 01 tenancy core.

ALTER TABLE IF EXISTS platform.idempotency_keys DROP CONSTRAINT IF EXISTS idempotency_keys_pkey;
ALTER TABLE IF EXISTS platform.idempotency_keys ADD PRIMARY KEY (key);

DROP TABLE IF EXISTS platform.audit_log;
DROP TABLE IF EXISTS platform.outbox_dlq;
DROP TABLE IF EXISTS platform.processed_events;

DROP TABLE IF EXISTS tenancy.reserved_slugs;
DROP TABLE IF EXISTS tenancy.slug_reservations;
DROP TABLE IF EXISTS tenancy.tenant_settings;
DROP TABLE IF EXISTS tenancy.tenants;

DROP SCHEMA IF EXISTS tenancy;

DROP FUNCTION IF EXISTS platform.enable_tenant_rls(text, text);
