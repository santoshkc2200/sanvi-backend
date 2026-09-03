-- Index supporting the payout_status filter on the payments list and CSV export.
--
-- Context: payments. Reversible counterpart:
-- 20260903000000_payments_payout_status_idx.down.sql

CREATE INDEX payments_tenant_payout_status_idx
  ON payments.payments (tenant_id, payout_status);
