DROP TABLE IF EXISTS platform_admin.application_fees;
DROP TABLE IF EXISTS payments.tax_settings;
ALTER TABLE payments.payments DROP CONSTRAINT IF EXISTS payments_payout_status_check;
ALTER TABLE payments.payments DROP COLUMN IF EXISTS payout_status;
DROP INDEX IF EXISTS payments.payouts_tenant_idx;
DROP INDEX IF EXISTS payments.payouts_external_idx;
DROP TABLE IF EXISTS payments.payouts;
