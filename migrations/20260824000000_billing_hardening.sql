-- Phase 04 hardening: durable Checkout idempotency and provider event cursors.

-- One durable Checkout state per tenant. The command id, redirect URLs and
-- integration suffix are persisted before calling Stripe, so concurrent or
-- retried requests reuse the same Stripe idempotency keys. The customer id is
-- retained across expired Checkout Sessions and future re-subscriptions.
CREATE TABLE billing.checkout_states (
  tenant_id                    bigint        PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  price_id                     bigint        NOT NULL REFERENCES billing.plan_prices(id),
  command_id                   uuid        NOT NULL,
  stripe_customer_id           text        UNIQUE,
  stripe_session_id            text        UNIQUE,
  session_url                  text,
  success_url                  text        NOT NULL,
  cancel_url                   text        NOT NULL,
  integration_identifier_suffix text      NOT NULL,
  expires_at                   timestamptz NOT NULL,
  created_at                   timestamptz NOT NULL DEFAULT now(),
  updated_at                   timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT checkout_states_session_pair_check CHECK (
    (stripe_session_id IS NULL AND session_url IS NULL)
    OR (stripe_session_id IS NOT NULL AND session_url IS NOT NULL)
  )
);

SELECT platform.enable_tenant_rls('billing', 'checkout_states');
GRANT SELECT, INSERT, UPDATE, DELETE ON billing.checkout_states TO sanvi_app;

-- Provider timestamps must not share the local updated_at column: Stripe
-- events are normally created before the worker processes them.
ALTER TABLE billing.subscriptions
  ADD COLUMN stripe_subscription_event_at timestamptz,
  ADD COLUMN stripe_invoice_event_at timestamptz;

-- Invoice projection updates also need their own out-of-order cursor.
ALTER TABLE billing.invoices
  ADD COLUMN stripe_event_created_at timestamptz;

-- Override revocation removes its projection entirely. The original phase-04
-- grant omitted DELETE, so the repository could clear entitlements but could
-- never remove the row under the application role.
GRANT DELETE ON billing.subscriptions TO sanvi_app;

-- Existing installations already have the system roles, so make the phase-04
-- permissions reachable during migration as well as on the next seed run.
INSERT INTO access.role_permissions (role_id, permission)
SELECT r.id, defaults.permission
FROM access.roles r
JOIN (VALUES
  ('platform_owner',    'platform.plan.write'),
  ('platform_owner',    'platform.subscription.override'),
  ('platform_operator', 'platform.plan.write'),
  ('platform_operator', 'platform.subscription.override'),
  ('tenant_owner',      'billing.subscription.create'),
  ('tenant_owner',      'billing.invoice.read'),
  ('tenant_billing',    'billing.subscription.create'),
  ('tenant_billing',    'billing.invoice.read')
) AS defaults(role_key, permission) ON defaults.role_key = r.key
WHERE r.is_system AND r.tenant_id IS NULL
ON CONFLICT DO NOTHING;
