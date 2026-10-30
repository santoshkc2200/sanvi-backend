-- Accounting contraction: the standalone accounting-service owns these tables in
-- its own database, the platform gateway forwards every Accounting route there,
-- and nothing in this repository reads or writes them any more.
--
-- expand-contract: no platform binary reads these tables after the gateway
-- release (20261028000100/200); the service's database is authoritative.
-- Offboarding exports, archival, retention and backups of Accounting data are
-- the service's own concern from here on.

-- Children before parents: postings, provider_credentials, oauth_states, and
-- audit_commands reference books.
DROP TABLE accounting.audit_commands;
DROP TABLE accounting.provider_credentials;
DROP TABLE accounting.tenant_secret_keys;
DROP TABLE accounting.oauth_states;
DROP TABLE accounting.postings;
DROP TABLE accounting.books;

DROP FUNCTION accounting.audit_commands_immutable();
DROP FUNCTION accounting.postings_terminal_immutable();

-- Not CASCADE: anything left in the schema is unexpected and should fail here.
DROP SCHEMA accounting;
