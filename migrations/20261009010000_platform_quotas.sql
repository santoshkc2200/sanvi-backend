-- PostgreSQL is authoritative; Redis is a disposable projection.
CREATE TABLE platform.quota_counters (
    tenant_id bigint NOT NULL REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
    dimension text NOT NULL CHECK (dimension IN ('api_requests','tracked_conversions','storage_bytes','custom_domains','seats','notification_sends')),
    period_start date NOT NULL,
    used bigint NOT NULL DEFAULT 0 CHECK (used >= 0),
    version bigint NOT NULL DEFAULT 0 CHECK (version >= 0),
    PRIMARY KEY (tenant_id,dimension,period_start)
);
SELECT platform.enable_tenant_rls('platform','quota_counters');
GRANT SELECT, INSERT, UPDATE, DELETE ON platform.quota_counters TO sanvi_app;
CREATE TABLE platform.quota_policies (
    tenant_id bigint PRIMARY KEY REFERENCES tenancy.tenants(id) ON DELETE CASCADE,
    enforce boolean NOT NULL DEFAULT false,
    limits jsonb NOT NULL,
    upgrade_path text NOT NULL
);
SELECT platform.enable_tenant_rls('platform','quota_policies');
GRANT SELECT, INSERT, UPDATE, DELETE ON platform.quota_policies TO sanvi_app;
-- Runs inside producer transactions. Atomic locks prevent oversubscription.
CREATE FUNCTION platform.record_quota_counter(p_tenant bigint,p_dimension text,p_period date,p_delta bigint)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    v_enforce boolean := false;
    v_limit bigint;
    v_upgrade text;
    v_used bigint;
BEGIN
    -- Do not recreate counters during cascading tenant erasure.
    IF NOT EXISTS (SELECT 1 FROM tenancy.tenants WHERE id=p_tenant) THEN RETURN; END IF;
    SELECT enforce,(limits->>p_dimension)::bigint,upgrade_path
      INTO v_enforce,v_limit,v_upgrade FROM platform.quota_policies
      WHERE tenant_id=p_tenant FOR SHARE;
    INSERT INTO platform.quota_counters (tenant_id,dimension,period_start,used)
      VALUES (p_tenant,p_dimension,p_period,0) ON CONFLICT DO NOTHING;
    SELECT used INTO v_used FROM platform.quota_counters
      WHERE tenant_id=p_tenant AND dimension=p_dimension AND period_start=p_period FOR UPDATE;
    IF COALESCE(v_enforce,false) AND p_delta>0 AND v_used+p_delta>v_limit THEN
      RAISE EXCEPTION 'quota_exceeded|%|%|%|%',p_dimension,v_used+p_delta,v_limit,v_upgrade
        USING ERRCODE='P0001';
    END IF;
    UPDATE platform.quota_counters SET used=GREATEST(0,used+p_delta),version=version+1
      WHERE tenant_id=p_tenant AND dimension=p_dimension AND period_start=p_period;
END;
$$;
REVOKE ALL ON FUNCTION platform.record_quota_counter(bigint,text,date,bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION platform.record_quota_counter(bigint,text,date,bigint) TO sanvi_app;
