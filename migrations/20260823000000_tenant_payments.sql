-- Phase 09: tenant payment providers (Stripe Connect, direct charges).
--
-- Context: payments. Reversible counterpart:
-- 20260823000000_tenant_payments.down.sql
--
-- Invariants (docs/phase-09-tenant-payments/):
-- - Sanvi is never the merchant of record. These tables record
--   identifiers, statuses and amounts only — no card data, no bank
--   details, no onboarding PII (those live at the provider).
-- - Money columns are `bigint` minor units + a `text` ISO-4217 code,
--   never `numeric` and never floats. JPY is zero-decimal; every amount
--   is an integer count of the currency's own minor unit.
-- - One live connection per (tenant, provider): the partial unique index
--   `one_active_provider_per_tenant` counts only rows with
--   `disconnected_at IS NULL`, so history is retained without blocking a
--   reconnect.
-- - `webhook_events` is platform-scoped: Connect events arrive before a
--   tenant is known (they are mapped via `external_account_id`), so the
--   table deliberately has NO tenant RLS policy — tenant-scoped
--   connections must not read it, and the application only touches it
--   through platform-scope transactions (09.3).

CREATE SCHEMA IF NOT EXISTS payments;

-- ---------------------------------------------------------------------------
-- The provider-connection aggregate.
-- ---------------------------------------------------------------------------

CREATE TABLE payments.provider_connections (
  id                   bigint        PRIMARY KEY,
  tenant_id            bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  provider             text        NOT NULL,
  external_account_id  text        NOT NULL,
  status               text        NOT NULL,
  capabilities         jsonb       NOT NULL DEFAULT '{}',
  requirements         jsonb       NOT NULL DEFAULT '{}',
  country              text,
  default_currency     text,
  connected_at         timestamptz,
  last_synced_at       timestamptz,
  disconnected_at      timestamptz,
  created_at           timestamptz NOT NULL DEFAULT now(),
  updated_at           timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT provider_connections_provider_check
    CHECK (provider IN ('stripe_connect')),
  CONSTRAINT provider_connections_status_check
    CHECK (status IN ('pending', 'onboarding', 'restricted', 'active',
                      'rejected', 'disconnected')),
  CONSTRAINT provider_connections_currency_check
    CHECK (default_currency IS NULL OR default_currency ~ '^[A-Z]{3}$')
);

-- One provider account may belong to only one connection row.
CREATE UNIQUE INDEX provider_connections_account_idx
  ON payments.provider_connections (provider, external_account_id);

-- Exactly one live connection per (tenant, provider).
CREATE UNIQUE INDEX one_active_provider_per_tenant
  ON payments.provider_connections (tenant_id, provider)
  WHERE disconnected_at IS NULL;

CREATE INDEX provider_connections_tenant_idx ON payments.provider_connections (tenant_id);
CREATE INDEX provider_connections_status_idx
  ON payments.provider_connections (status, updated_at);

SELECT platform.enable_tenant_rls('payments', 'provider_connections');

-- ---------------------------------------------------------------------------
-- The checkout aggregate (direct charges on the connected account, 09.4).
-- ---------------------------------------------------------------------------

CREATE TABLE payments.checkouts (
  id                    bigint        PRIMARY KEY,
  tenant_id             bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  connection_id         bigint        NOT NULL REFERENCES payments.provider_connections(id) ON DELETE CASCADE,
  reference             text        NOT NULL,
  customer_ref          jsonb,
  amount_minor          bigint      NOT NULL CHECK (amount_minor >= 0),
  currency              text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  status                text        NOT NULL,
  external_session_id   text,
  idempotency_key       uuid,
  locale                text,
  expires_at            timestamptz,
  completed_at          timestamptz,
  created_at            timestamptz NOT NULL DEFAULT now(),
  updated_at            timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT checkouts_status_check
    CHECK (status IN ('pending', 'open', 'complete', 'expired', 'canceled'))
);

CREATE UNIQUE INDEX checkouts_idempotency_idx
  ON payments.checkouts (tenant_id, idempotency_key)
  WHERE idempotency_key IS NOT NULL;

CREATE INDEX checkouts_tenant_idx ON payments.checkouts (tenant_id);
CREATE INDEX checkouts_status_idx ON payments.checkouts (status, updated_at);
CREATE INDEX checkouts_external_idx ON payments.checkouts (external_session_id)
  WHERE external_session_id IS NOT NULL;

SELECT platform.enable_tenant_rls('payments', 'checkouts');

-- ---------------------------------------------------------------------------
-- The payment read model (webhook-driven truth, 09.4/09.5).
-- ---------------------------------------------------------------------------

CREATE TABLE payments.payments (
  id                    bigint        PRIMARY KEY,
  tenant_id             bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  checkout_id           bigint        REFERENCES payments.checkouts(id) ON DELETE SET NULL,
  external_payment_id   text        NOT NULL,
  status                text        NOT NULL,
  amount_minor          bigint      NOT NULL CHECK (amount_minor >= 0),
  currency              text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  fee_amount_minor      bigint      CHECK (fee_amount_minor IS NULL OR fee_amount_minor >= 0),
  fee_currency          text        CHECK (fee_currency IS NULL OR fee_currency ~ '^[A-Z]{3}$'),
  failure               jsonb,
  captured_at           timestamptz,
  created_at            timestamptz NOT NULL DEFAULT now(),
  updated_at            timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT payments_status_check
    CHECK (status IN ('requires_action', 'processing', 'succeeded', 'failed', 'canceled'))
);

CREATE UNIQUE INDEX payments_external_idx
  ON payments.payments (tenant_id, external_payment_id);

CREATE INDEX payments_tenant_idx ON payments.payments (tenant_id);
CREATE INDEX payments_status_idx ON payments.payments (status, updated_at);

SELECT platform.enable_tenant_rls('payments', 'payments');

-- ---------------------------------------------------------------------------
-- The refund read model (09.5). Partial refunds are guarded by the domain
-- (portion ≤ total) before any provider call; the columns stay plain
-- minor units so JPY needs no special handling here.
-- ---------------------------------------------------------------------------

CREATE TABLE payments.refunds (
  id                   bigint        PRIMARY KEY,
  tenant_id            bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  payment_id           bigint        NOT NULL REFERENCES payments.payments(id) ON DELETE CASCADE,
  external_refund_id   text,
  amount_minor         bigint      NOT NULL CHECK (amount_minor >= 0),
  currency             text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  reason               text,
  status               text        NOT NULL,
  created_by           bigint,
  idempotency_key      uuid,
  created_at           timestamptz NOT NULL DEFAULT now(),
  updated_at           timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT refunds_status_check
    CHECK (status IN ('pending', 'succeeded', 'failed'))
);

CREATE UNIQUE INDEX refunds_external_idx
  ON payments.refunds (tenant_id, external_refund_id)
  WHERE external_refund_id IS NOT NULL;

CREATE UNIQUE INDEX refunds_idempotency_idx
  ON payments.refunds (tenant_id, idempotency_key)
  WHERE idempotency_key IS NOT NULL;

CREATE INDEX refunds_payment_idx ON payments.refunds (payment_id);

SELECT platform.enable_tenant_rls('payments', 'refunds');

-- ---------------------------------------------------------------------------
-- Connected-account webhook events (09.3): the source of truth about
-- money. Platform-scoped by design — see the invariants block above. Not
-- registered with `enable_tenant_rls`: events arrive before the tenant is
-- known and map to one only through `external_account_id`; the
-- application accesses this table exclusively through platform-scope
-- transactions.
-- ---------------------------------------------------------------------------

CREATE TABLE payments.webhook_events (
  stripe_event_id   text        PRIMARY KEY,
  account_id        text,
  type              text        NOT NULL,
  payload           jsonb       NOT NULL,
  api_version       text,
  received_at       timestamptz NOT NULL DEFAULT now(),
  processed_at      timestamptz,
  attempts          int         NOT NULL DEFAULT 0,
  error             text,
  created           boolean     NOT NULL DEFAULT false
);

CREATE INDEX webhook_events_unprocessed_idx
  ON payments.webhook_events (received_at)
  WHERE processed_at IS NULL AND attempts < 5;

CREATE INDEX webhook_events_account_idx ON payments.webhook_events (account_id);

-- ---------------------------------------------------------------------------
-- Least-privilege grants for the NOBYPASSRLS application role.
-- ---------------------------------------------------------------------------

GRANT USAGE ON SCHEMA payments TO sanvi_app;

GRANT SELECT, INSERT, UPDATE ON payments.provider_connections TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON payments.checkouts TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON payments.payments TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON payments.refunds TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON payments.webhook_events TO sanvi_app;
