-- Revert phase 02: identity & access schemas.
--
-- NOTE: `platform.enable_tenant_rls` keeps its v2 body (with the audited
-- `sanvi.platform_scope` escape hatch) — reverting the function while
-- tenancy.tenant_settings still uses it would be a behavioural change to
-- phase-01 data access; the function body itself is data-free policy.

DROP TABLE IF EXISTS identity.link_intents;
DROP TABLE IF EXISTS identity.invitations;
DROP TABLE IF EXISTS identity.linked_accounts;
DROP TABLE IF EXISTS identity.membership_roles;
DROP TABLE IF EXISTS identity.memberships;
DROP TABLE IF EXISTS identity.users;

DROP TABLE IF EXISTS access.role_permissions;
DROP TABLE IF EXISTS access.roles;

DROP SCHEMA IF EXISTS identity;
DROP SCHEMA IF EXISTS access;
