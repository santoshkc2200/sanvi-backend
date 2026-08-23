-- Reverse 09.7: remove the payments statutory retention override row.
DELETE FROM privacy.retention_rules WHERE data_class = 'transaction_record';
