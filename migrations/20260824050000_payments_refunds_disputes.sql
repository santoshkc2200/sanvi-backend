-- Phase 09.5: refunds, disputes and payment list support.
--
-- Context: payments. Reversible counterpart:
-- 20260824050000_payments_refunds_disputes.down.sql
--
-- - Disputes are read-only: status, amount, reason, response deadline —
--   no evidence ever stored at Sanvi (TASK-006).
-- - Payment list indexes cover the documented filter set: date range,
--   status, currency and (via the checkout join) customer reference.
--   Cursor pagination uses (created_at, id) so the index is tenant +
--   created_at + id.
-- - No column outside this file mentions evidence, card data, bank
--   details or PII — those live at the provider.

-- ---------------------------------------------------------------------------
-- Disputes (read-only projection of charge.dispute.*).
-- ---------------------------------------------------------------------------

CREATE TABLE payments.disputes (
  id                   bigint        PRIMARY KEY,
  tenant_id            bigint        NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  payment_id           bigint        NOT NULL REFERENCES payments.payments(id) ON DELETE CASCADE,
  external_dispute_id  text        NOT NULL,
  status               text        NOT NULL,
  amount_minor         bigint      NOT NULL CHECK (amount_minor >= 0),
  currency             text        NOT NULL CHECK (currency ~ '^[A-Z]{3}$'),
  reason               text,
  due_by               timestamptz,
  charge_id            text,
  created_at           timestamptz NOT NULL DEFAULT now(),
  updated_at           timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT disputes_status_check
    CHECK (status ~ '^[a-z_]+$')
);

CREATE UNIQUE INDEX disputes_external_idx
  ON payments.disputes (tenant_id, external_dispute_id);

CREATE INDEX disputes_payment_idx ON payments.disputes (payment_id);
CREATE INDEX disputes_tenant_idx ON payments.disputes (tenant_id);
CREATE INDEX disputes_status_idx ON payments.disputes (tenant_id, status);
CREATE INDEX disputes_created_idx ON payments.disputes (tenant_id, created_at DESC, id DESC);

SELECT platform.enable_tenant_rls('payments', 'disputes');

-- ---------------------------------------------------------------------------
-- Payment list indexes — cover every documented filter and the cursor.
-- ---------------------------------------------------------------------------

CREATE INDEX payments_tenant_created_id_idx
  ON payments.payments (tenant_id, created_at DESC, id DESC);

CREATE INDEX payments_tenant_currency_idx
  ON payments.payments (tenant_id, currency);

-- Customer substring search is over checkouts.customer_ref JSON text.
-- This GIN index does NOT serve the implemented `customer_ref::text
-- ILIKE '%…%'` predicate (jsonb_ops GIN supports @> containment, not
-- text matching): the filter runs as an RLS-bounded scan over the
-- tenant's own checkouts, which stays reasonable at per-tenant scale.
-- If customer search ever needs to go wide, add a pg_trgm expression
-- index on (customer_ref::text) — a deliberate follow-up, not a silent
-- assumption. (The list query joins payments → checkouts when a
-- customer filter is present.)
CREATE INDEX checkouts_customer_ref_idx
  ON payments.checkouts USING gin (customer_ref);

-- ---------------------------------------------------------------------------
-- Least-privilege grants.
-- ---------------------------------------------------------------------------

GRANT SELECT, INSERT, UPDATE ON payments.disputes TO sanvi_app;
