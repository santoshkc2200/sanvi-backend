-- Runs once, as the postgres superuser, on first container start.
-- Mirrors production role separation: the application role cannot create
-- tables and cannot bypass row-level security; migrations run under a
-- separate owner role.

CREATE ROLE sanvi_migrator LOGIN PASSWORD 'migrator-local-dev'
  NOSUPERUSER NOCREATEDB NOCREATEROLE;

CREATE ROLE sanvi_app LOGIN PASSWORD 'app-local-dev'
  NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;

CREATE DATABASE sanvi OWNER sanvi_migrator;

\connect sanvi
GRANT CONNECT ON DATABASE sanvi TO sanvi_app;
