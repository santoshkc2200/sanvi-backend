-- pg_stat_statements: the preload library is set by the compose command
-- flag (docker-compose.yml, TASK-021 step 0); this creates the extension on
-- the first boot of a fresh volume. A volume created before the flag landed
-- needs the preload first, then a one-off:
--   CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
--
-- template1 as well as the default database: pg_stat_statements must exist
-- in the database it is queried from, and the scale seed (and any database
-- created later) inherits from template1 — the 2026-09-17 baseline missed
-- this and had to create the extension per capture.
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
\connect template1
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
