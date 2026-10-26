CREATE SCHEMA IF NOT EXISTS media;
GRANT USAGE ON SCHEMA media TO sanvi_app;
CREATE TABLE media.subject_namespaces (
 subject_key text NOT NULL, tenant_id bigint NOT NULL, owner_ref text NOT NULL,
 created_at timestamptz NOT NULL DEFAULT now(),
 PRIMARY KEY (subject_key, tenant_id, owner_ref)
);
GRANT SELECT, INSERT, DELETE ON media.subject_namespaces TO sanvi_app;
ALTER TABLE media.subject_namespaces ENABLE ROW LEVEL SECURITY;
ALTER TABLE media.subject_namespaces FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON media.subject_namespaces
USING (current_setting('sanvi.platform_scope', true) = 'on' OR tenant_id = NULLIF(current_setting('sanvi.tenant_id', true), '')::bigint)
WITH CHECK (current_setting('sanvi.platform_scope', true) = 'on' OR tenant_id = NULLIF(current_setting('sanvi.tenant_id', true), '')::bigint);
