-- Monotonic per-membership version, bumped on every write.
--
-- The access context's Keto sync consumer needs a total order over membership
-- changes for one (tenant_id, user_id) so a redelivered or reordered event
-- cannot overwrite a newer state with an older one. Wall-clock timestamps
-- cannot provide that: the API runs on several replicas and their clocks skew.
-- This counter is incremented inside the same transaction as the role change,
-- under the row lock, so it is monotonic regardless of which replica writes.
ALTER TABLE identity.memberships
  ADD COLUMN version bigint NOT NULL DEFAULT 0;
