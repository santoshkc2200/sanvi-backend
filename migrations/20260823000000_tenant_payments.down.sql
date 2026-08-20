-- Phase 09 down: drop the payments schema. Empty by construction while the
-- feature flag defaults off (rollback plan: flag off; the migration stays
-- because empty tables are harmless — this file is the rehearsal path).
DROP SCHEMA IF EXISTS payments CASCADE;
