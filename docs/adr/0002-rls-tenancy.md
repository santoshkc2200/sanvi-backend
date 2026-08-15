# ADR 0002 — Row-level security for tenant isolation

- **Status:** accepted
- **Date:** 2026-08-15
- **Deciders:** platform engineering

## Context

Multi-tenancy is the core risk surface of the platform: a tenant-data leak is
a trust-ending event. Application-level `WHERE tenant_id = ?` filtering has
failed repeatedly across the industry because one forgotten predicate in one
query is enough to break isolation.

## Decision

Every tenant-scoped table carries `tenant_id` and is protected by a
**row-level security policy** reading `current_setting('sanvi.tenant_id')`.
The transaction manager sets the context with `SET LOCAL` at the start of
every request transaction. The application role is `NOBYPASSRLS`; migrations
run under a separate owner role, so no code path the API runs can ever
bypass the policy. Tests enumerate every tenant-scoped table from the
catalog and assert tenant A cannot read tenant B's rows, so new tables are
covered automatically.

## Consequences

- **Positive:** isolation enforced by the database, not by review; least
  privilege is structural (`NOBYPASSRLS`); failures are loud and tested.
- **Negative:** every query goes through the transaction manager; cross-tenant
  reporting (platform admin) needs explicit, audited bypass patterns
  (separate read role, phase 03).
