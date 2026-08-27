-- Phase 09.6: payouts, tax liability and the application fee (TASK-007).
--
-- Context: payments (+ platform_admin for the platform's own fee config).
-- Reversible counterpart:
-- 20260824060000_payments_payouts_tax_fees.down.sql
--
-- Invariants:
-- - Payouts are read-only at Sanvi: the projection records what the
--   provider reported (`payout.paid` / `payout.failed`), never moves
--   money. A failed payout is an alert to the tenant, not a silent row.
-- - `payments.payments.payout_status` is the "payout status" column of
--   the payment list: set only from payout events that name a source
--   transaction we can map back to a stored payment.
-- - Tax settings store *our* toggle and the provider's compliance
--   snapshot. Registration and remittance are the tenant's legal
--   responsibility; these columns exist so a lapsed registration can
--   warn instead of silently under-collecting.
-- - The application-fee configuration lives in `platform_admin` (the
--   platform's own fee is never tenant-settable) as pure data: disabling
--   per tenant needs no deploy. It ships disabled everywhere — no
--   `application_fee_amount` on any PaymentIntent in 0.10.0.

-- ---------------------------------------------------------------------------
-- The payout projection (webhook-driven truth, 09.6).
-- ---------------------------------------------------------------------------

CREATE TABLE payments.payouts (
  id                  bigint        PRIMARY KEY,
  tenant_id           bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  connection_id       bigint        REFERENCES payments.provider_connections(id) ON DELETE SET NULL,
  external_payout_id  text        NOT NULL,
  amount_minor        bigint      NOT NULL CHECK (amount_minor >= 0),
  currency            text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  status              text        NOT NULL,
  arrival_at          timestamptz,
  created_at          timestamptz NOT NULL DEFAULT now(),
  updated_at          timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT payouts_status_check
    CHECK (status IN ('pending', 'paid', 'failed'))
);

CREATE UNIQUE INDEX payouts_external_idx
  ON payments.payouts (tenant_id, external_payout_id);

CREATE INDEX payouts_tenant_idx ON payments.payouts (tenant_id);

SELECT platform.enable_tenant_rls('payments', 'payouts');

-- ---------------------------------------------------------------------------
-- The payout status column on the payment read model (the TASK-006 list's
-- "payout status"): NULL until payout reconciliation includes this
-- payment's balance transaction.
-- ---------------------------------------------------------------------------

ALTER TABLE payments.payments ADD COLUMN payout_status text;
ALTER TABLE payments.payments ADD CONSTRAINT payments_payout_status_check
  CHECK (payout_status IS NULL OR payout_status IN ('pending', 'paid', 'failed'));

-- ---------------------------------------------------------------------------
-- Per-tenant automatic-tax settings (09.6). One row per tenant: with one
-- live connection per provider, liability always routes to that account.
-- `enabled` flips only through the preflight; the stored snapshot is what
-- the scheduled re-check warns from.
-- ---------------------------------------------------------------------------

CREATE TABLE payments.tax_settings (
  tenant_id           bigint        PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  enabled             boolean     NOT NULL DEFAULT false,
  liability_account   text        NOT NULL DEFAULT '',
  provider_status     text        NOT NULL DEFAULT 'unknown',
  registration_count  integer     NOT NULL DEFAULT 0 CHECK (registration_count >= 0),
  warning             jsonb,
  last_checked_at     timestamptz,
  created_at          timestamptz NOT NULL DEFAULT now(),
  updated_at          timestamptz NOT NULL DEFAULT now()
);

SELECT platform.enable_tenant_rls('payments', 'tax_settings');

-- ---------------------------------------------------------------------------
-- The per-tenant application-fee configuration (platform-admin context).
-- The tenant cannot set the platform's own fee — hence outside the tenant
-- API entirely. `basis_points` + optional fixed amount, with a minimum
-- interpreted in the charge currency's minor units (currency-aware by
-- construction: JPY minor units are whole yen). Disabled by default.
-- ---------------------------------------------------------------------------

CREATE TABLE platform_admin.application_fees (
  tenant_id           bigint        PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  basis_points        integer     CHECK (basis_points IS NULL OR (basis_points > 0 AND basis_points <= 10000)),
  fixed_amount_minor  bigint      CHECK (fixed_amount_minor IS NULL OR fixed_amount_minor >= 0),
  fixed_currency      text        CHECK (fixed_currency IS NULL OR fixed_currency ~ '^[A-Z]{3}$'),
  minimum_minor       bigint      NOT NULL DEFAULT 0 CHECK (minimum_minor >= 0),
  enabled             boolean     NOT NULL DEFAULT false,
  set_by              bigint,
  reason              text,
  created_at          timestamptz NOT NULL DEFAULT now(),
  updated_at          timestamptz NOT NULL DEFAULT now()
);

-- Standard tenant isolation: operator reads/writes run under the audited
-- platform scope; a tenant connection could only ever see its own row
-- (and has no API that does).
SELECT platform.enable_tenant_rls('platform_admin', 'application_fees');

-- ---------------------------------------------------------------------------
-- The application role's grants (same vocabulary as every payments table:
-- no DELETE — history is retained; tax_settings and payouts are read and
-- maintained by the webhook paths under the audited platform scope).
-- ---------------------------------------------------------------------------

GRANT SELECT, INSERT, UPDATE ON payments.payouts TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON payments.tax_settings TO sanvi_app;
GRANT SELECT, INSERT, UPDATE ON platform_admin.application_fees TO sanvi_app;
