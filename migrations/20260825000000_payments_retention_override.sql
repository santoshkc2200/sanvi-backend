-- Phase 09.7: the payments statutory retention override (TASK-008).
--
-- Context: privacy (retention framework seed). Reversible counterpart:
-- 20260825000000_payments_retention_override.down.sql
--
-- Payment transaction records on a tenant's connected account fall under
-- the phase-05 retention framework with a statutory override: financial
-- records outlive an erasure request — tax and accounting law requires
-- the transaction record even when the payer is erased around it. The
-- compile-time half lives in sanvi_privacy::domain::DATA_CLASSES; this
-- row is the operational half the notice's retention table is generated
-- from. The nightly sweep never touches these rows (Archive action, and
-- erasure reports them as retained with this reason).
--
-- No schema change: `privacy.retention_rules.data_class` is free text.

INSERT INTO privacy.retention_rules
  (data_class, sensitivity, basis, period, action, disclosed_in_notice)
VALUES
  ('transaction_record', 'sensitive', 'statutory_retention',
   make_interval(days => 3650), 'archive', true)
ON CONFLICT (data_class) DO NOTHING;
