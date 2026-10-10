-- Down: drop the archival registry. The cold objects on the storage
-- volume are outside the database; the offboarding runbook covers their
-- sweep when the feature is decommissioned.
DROP TABLE IF EXISTS platform.archives;
