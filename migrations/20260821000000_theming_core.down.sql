-- Reversible counterpart of 20260821000000_theming_core.sql.
-- Drops the whole theming schema (tenant data is cascade-deleted with the
-- tenant lifecycle; themes/versions are reference data reinstalled by
-- `theme publish` or re-seeded by the migration).

DROP SCHEMA IF EXISTS theming CASCADE;
