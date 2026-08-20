-- Persist the operator who rejected a DSR so an appeal cannot be reviewed
-- by the same operator.
ALTER TABLE privacy.data_subject_requests
  ADD COLUMN rejected_by uuid;
