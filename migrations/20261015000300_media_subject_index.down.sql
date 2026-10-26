-- Preserve populated ownership evidence; reverting requires an approved data transition.
DO $$ BEGIN
 IF EXISTS (SELECT 1 FROM media.subject_namespaces) THEN
  RAISE EXCEPTION 'cannot drop populated media ownership index';
 END IF;
END $$;
DROP TABLE media.subject_namespaces;
