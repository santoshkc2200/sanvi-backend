-- Reversal of 20260819000000_privacy_compliance.sql.
-- Drops the privacy schema in reverse dependency order; the shared
-- platform.enable_tenant_rls helper is left in place (owned by earlier
-- phases).

DROP TABLE IF EXISTS privacy.request_metrics_yearly;
DROP TABLE IF EXISTS privacy.breach_obligations;
DROP TABLE IF EXISTS privacy.breach_incidents;
DROP TABLE IF EXISTS privacy.export_artifacts;
DROP TABLE IF EXISTS privacy.subject_keys;
DROP TABLE IF EXISTS privacy.subprocessors;
DROP TABLE IF EXISTS privacy.risk_assessments;
DROP TABLE IF EXISTS privacy.processing_activities;
DROP TABLE IF EXISTS privacy.legal_holds;
DROP TABLE IF EXISTS privacy.retention_rules;
DROP TABLE IF EXISTS privacy.purpose_directives;
DROP TABLE IF EXISTS privacy.preference_signals;
DROP TABLE IF EXISTS privacy.consents;
DROP TABLE IF EXISTS privacy.verification_challenges;
DROP TABLE IF EXISTS privacy.request_participants;
DROP TABLE IF EXISTS privacy.request_appeals;
DROP TABLE IF EXISTS privacy.authorized_agents;
DROP TABLE IF EXISTS privacy.data_subject_requests;
DROP TABLE IF EXISTS privacy.data_subjects;
DROP TABLE IF EXISTS privacy.jurisdiction_profiles;
DROP SCHEMA IF EXISTS privacy;
