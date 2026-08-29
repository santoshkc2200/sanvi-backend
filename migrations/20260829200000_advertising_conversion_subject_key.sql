-- 10.6 retry slice: the withdrawal re-check before a manual retry needs the
-- capturing subject's identity back, but raw PII (email/phone) must never be
-- persisted in this context (TASK-014). For device/end-user/platform-user
-- subjects the key material (an opaque cookie ref, the tenant's own customer
-- ref, or a Snowflake user id) is not itself sensitive and is stored as-is;
-- for a contact (email) subject nothing reconstructable is stored, and the
-- retry path refuses rather than re-deriving a directive answer for the
-- wrong subject.
ALTER TABLE advertising.conversion_events
  ADD COLUMN subject_key jsonb NOT NULL DEFAULT '{"kind":"contact"}'::jsonb;
