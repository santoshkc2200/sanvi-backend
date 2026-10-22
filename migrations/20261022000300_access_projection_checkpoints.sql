-- Completed external authorization snapshots. An event is only a wake-up:
-- consumers reload authoritative rows under per-aggregate transaction locks.
CREATE TABLE access.membership_projection_checkpoints (
    tenant_id bigint NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
    user_id bigint NOT NULL,
    role_objects text[] NOT NULL DEFAULT '{}',
    applied_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, user_id)
);
SELECT platform.enable_tenant_rls('access', 'membership_projection_checkpoints');
CREATE TABLE access.role_projection_checkpoints (
    role_id bigint PRIMARY KEY,
    role_objects text[] NOT NULL DEFAULT '{}',
    permissions text[] NOT NULL DEFAULT '{}',
    applied_at timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT, INSERT, UPDATE, DELETE ON access.membership_projection_checkpoints,
    access.role_projection_checkpoints TO sanvi_app;
