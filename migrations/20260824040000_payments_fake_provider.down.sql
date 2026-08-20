-- Revert 20260824040000_payments_fake_provider.sql: the connection table
-- accepts only the Stripe Connect kind again. Any 'fake' rows must be
-- gone before this runs (non-production data only, by construction).

ALTER TABLE payments.provider_connections
  DROP CONSTRAINT provider_connections_provider_check;

ALTER TABLE payments.provider_connections
  ADD CONSTRAINT provider_connections_provider_check
  CHECK (provider IN ('stripe_connect'));
