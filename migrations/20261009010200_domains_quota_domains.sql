CREATE FUNCTION domains.count_quota_domain() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='UPDATE' AND NEW.tenant_id=OLD.tenant_id
       AND (NEW.status <> 'removed')=(OLD.status <> 'removed') THEN RETURN NULL; END IF;
    IF TG_OP IN ('UPDATE','DELETE') AND OLD.status <> 'removed' THEN
      PERFORM platform.record_quota_counter(OLD.tenant_id,'custom_domains',DATE '1970-01-01',-1);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND NEW.status <> 'removed' THEN
      PERFORM platform.record_quota_counter(NEW.tenant_id,'custom_domains',DATE '1970-01-01',1);
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER quota_domains AFTER INSERT OR UPDATE OR DELETE ON domains.custom_domains
    FOR EACH ROW EXECUTE FUNCTION domains.count_quota_domain();
SELECT platform.record_quota_counter(tenant_id,'custom_domains',DATE '1970-01-01',count(*))
  FROM domains.custom_domains WHERE status <> 'removed' GROUP BY tenant_id;
