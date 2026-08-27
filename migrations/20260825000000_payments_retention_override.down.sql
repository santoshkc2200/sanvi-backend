-- Reverse 09.7 only when the forward migration inserted this exact seed.
-- An operator-owned pre-existing rule wins the forward ON CONFLICT and
-- must survive a rollback.
DELETE FROM privacy.retention_rules
WHERE data_class = 'transaction_record'
  AND updated_by = '97';
