ALTER TABLE privacy.data_subject_requests ADD COLUMN restriction_released_at timestamptz;
CREATE INDEX privacy_active_restrictions ON privacy.data_subject_requests ((subject_ref->>'key')) WHERE kind IN ('restriction', 'objection') AND verified_at IS NOT NULL AND status <> 'rejected' AND restriction_released_at IS NULL;
