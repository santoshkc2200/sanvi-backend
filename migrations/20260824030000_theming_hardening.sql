-- The privacy provider anonymises the actor on immutable theme snapshots.
-- Keep the row immutable to ordinary application writes while granting the
-- single column update required by the unscoped DSR workflow.
GRANT UPDATE (published_by) ON theming.tenant_theme_revisions TO sanvi_app;
