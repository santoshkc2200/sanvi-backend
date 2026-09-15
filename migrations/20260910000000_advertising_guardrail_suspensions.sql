-- TASK-017 review fix: durable state machine for budget-guardrail rollback
-- notices.
--
-- One row per tenant recording the last enforcement state the sweep
-- observed for it: 'serving' (the tenant's caps are being evaluated) or
-- 'suspended' (they are not, with the reason). A suspension notice goes out
-- exactly once per observed serving→suspended transition — surviving worker
-- restarts, shared by every replica, re-armed when enforcement serves the
-- tenant again, and re-armed by a reason change while suspended.
--
-- The state (not just the suspension) is what makes the notice honest: a
-- tenant observed suspended on the very first tick — a fresh deploy with
-- `features.advertising = false`, or a tenant that never held the
-- entitlement — has no enabled→suspended transition to announce, and must
-- not be emailed that its guardrails were "suspended". Only a tenant whose
-- row recorded 'serving' first gets the notice.
--
-- This replaces the per-process 24-hour timestamp, which re-notified daily,
-- reset on restart, and suppressed a *different* reason occurring within
-- the window.

CREATE TABLE advertising.guardrail_suspensions (
  tenant_id    bigint      PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
  state        text        NOT NULL DEFAULT 'suspended' CHECK (state IN ('serving', 'suspended')),
  reason       text        NOT NULL DEFAULT 'none',
  suspended_at timestamptz NOT NULL DEFAULT now(),
  notified_at  timestamptz
);
SELECT platform.enable_tenant_rls('advertising', 'guardrail_suspensions');

GRANT SELECT, INSERT, UPDATE, DELETE ON advertising.guardrail_suspensions TO sanvi_app;
