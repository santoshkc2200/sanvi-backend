-- Phase 03 down: drop the platform_admin and audit schemas and the access
-- tables introduced for features, entitlements and quota usage.

DROP TRIGGER IF EXISTS entries_no_update ON audit.entries;
DROP FUNCTION IF EXISTS audit.entries_append_only();
DROP TABLE IF EXISTS audit.entries;
DROP SCHEMA IF EXISTS audit;

DROP TABLE IF EXISTS platform_admin.pending_approvals;
DROP TABLE IF EXISTS platform_admin.impersonation_grants;
DROP SCHEMA IF EXISTS platform_admin;

DROP TABLE IF EXISTS access.quota_usage;
DROP TABLE IF EXISTS access.entitlements;
DROP TABLE IF EXISTS access.features;
