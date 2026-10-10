-- Down: drop the offboarding record. The purge itself is not reversible;
-- this only removes the bookkeeping.
DROP TABLE IF EXISTS tenancy.offboarding_requests;
