ALTER TABLE privacy.data_subject_requests DROP CONSTRAINT dsr_kind_check;
ALTER TABLE privacy.data_subject_requests ADD CONSTRAINT dsr_kind_check CHECK (kind IN ('access','export','erasure','rectification','restriction','objection','opt_out_sale_share','opt_out_targeted_advertising','opt_out_profiling','limit_sensitive_use','know_categories','other'));
CREATE TABLE privacy.manual_fulfillments (request_id bigint PRIMARY KEY REFERENCES privacy.data_subject_requests(id), summary text NOT NULL, evidence_reference text NOT NULL, completed_at timestamptz NOT NULL DEFAULT now());
GRANT SELECT, INSERT ON privacy.manual_fulfillments TO sanvi_app;
