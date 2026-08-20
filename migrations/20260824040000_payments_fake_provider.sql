-- Phase 09.1: admit the fake provider's kind on the connection table.
--
-- Context: payments. Reversible counterpart:
-- 20260824040000_payments_fake_provider.down.sql
--
-- The fake provider (crates/contexts/payments/src/adapters/fake_provider.rs)
-- is registered only in non-production environments and never listed in a
-- production catalog, but dev/test environments persist real connection
-- rows against it — the whole point of the adapter is letting the
-- frontend and the integration suite run connect → active → checkout →
-- refund before Stripe sandbox credentials exist. Those rows need the
-- CHECK to admit 'fake'; a value nothing in production can write, because
-- no production process can resolve the fake adapter through the
-- registry.
--
-- Stays in lockstep with ProviderKind (domain/provider.rs).

ALTER TABLE payments.provider_connections
  DROP CONSTRAINT provider_connections_provider_check;

ALTER TABLE payments.provider_connections
  ADD CONSTRAINT provider_connections_provider_check
  CHECK (provider IN ('stripe_connect', 'fake'));
