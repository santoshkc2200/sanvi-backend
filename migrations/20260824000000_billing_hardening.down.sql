-- Reverse phase 04 billing hardening.

ALTER TABLE billing.invoices
  DROP COLUMN IF EXISTS stripe_event_created_at;

ALTER TABLE billing.subscriptions
  DROP COLUMN IF EXISTS stripe_invoice_event_at,
  DROP COLUMN IF EXISTS stripe_subscription_event_at;

DROP TABLE IF EXISTS billing.checkout_states;

REVOKE DELETE ON billing.subscriptions FROM sanvi_app;

DELETE FROM access.role_permissions rp
USING access.roles r
WHERE rp.role_id = r.id
  AND r.is_system
  AND r.tenant_id IS NULL
  AND (r.key, rp.permission) IN (
    ('platform_owner',    'platform.plan.write'),
    ('platform_owner',    'platform.subscription.override'),
    ('platform_operator', 'platform.plan.write'),
    ('platform_operator', 'platform.subscription.override'),
    ('tenant_owner',      'billing.subscription.create'),
    ('tenant_owner',      'billing.invoice.read'),
    ('tenant_billing',    'billing.subscription.create'),
    ('tenant_billing',    'billing.invoice.read')
  );
