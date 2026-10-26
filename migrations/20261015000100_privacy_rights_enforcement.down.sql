-- An old binary cannot enforce these durable restrictions.
DO $$ BEGIN
 IF EXISTS (SELECT 1 FROM privacy.data_subject_requests WHERE kind IN ('restriction','objection') AND verified_at IS NOT NULL AND status <> 'rejected') THEN
  RAISE EXCEPTION 'cannot remove restriction enforcement while verified rights records exist';
 END IF;
END $$;
DROP INDEX privacy.privacy_active_restrictions;
ALTER TABLE privacy.data_subject_requests DROP COLUMN restriction_released_at;
