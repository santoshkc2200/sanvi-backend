DO $$ BEGIN IF EXISTS (SELECT 1 FROM privacy.data_subject_requests WHERE kind = 'other') OR EXISTS (SELECT 1 FROM privacy.manual_fulfillments) THEN RAISE EXCEPTION 'Manual requests must remain readable; retain this schema while records exist'; END IF; END $$;
DROP TABLE privacy.manual_fulfillments;
ALTER TABLE privacy.data_subject_requests DROP CONSTRAINT dsr_kind_check;
ALTER TABLE privacy.data_subject_requests ADD CONSTRAINT dsr_kind_check CHECK (kind IN ('access','export','erasure','rectification','restriction','objection','opt_out_sale_share','opt_out_targeted_advertising','opt_out_profiling','limit_sensitive_use','know_categories'));
