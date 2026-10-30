-- Payments contraction: the standalone payments-service owns these tables in
-- its own database, the platform gateway forwards every Payments route there,
-- and nothing in this repository reads or writes them any more.
--
-- expand-contract: no platform binary reads these tables after the gateway
-- release (20261028000500/600); the service's database is authoritative.
-- Offboarding exports, archival, retention and backups of Payments data are
-- the service's own concern from here on.

DELETE FROM platform.partitioned_tables
WHERE schema_name = 'payments';

-- Children before parents: refunds and disputes reference payments, payments
-- references checkouts, and checkouts and payouts reference connections.
DROP TABLE payments.refunds;
DROP TABLE payments.disputes;
DROP TABLE payments.payouts;
DROP TABLE payments.payments;
DROP TABLE payments.checkouts;
DROP TABLE payments.tax_settings;
DROP TABLE payments.provider_connections;
-- Dropping the partitioned parent drops its monthly partitions.
DROP TABLE payments.webhook_events;
DROP TABLE payments.webhook_event_dedup;

-- Not CASCADE: anything left in the schema is unexpected and should fail here.
DROP SCHEMA payments;
