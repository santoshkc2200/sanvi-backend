-- Active memberships are a gauge, not a monthly accumulator.
CREATE FUNCTION identity.count_quota_seat() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='UPDATE' AND NEW.tenant_id=OLD.tenant_id
       AND (NEW.status='active')=(OLD.status='active') THEN RETURN NULL; END IF;
    IF TG_OP IN ('UPDATE','DELETE') AND OLD.status='active' THEN
      PERFORM platform.record_quota_counter(OLD.tenant_id,'seats',DATE '1970-01-01',-1);
    END IF;
    IF TG_OP IN ('INSERT','UPDATE') AND NEW.status='active' THEN
      PERFORM platform.record_quota_counter(NEW.tenant_id,'seats',DATE '1970-01-01',1);
    END IF;
    RETURN NULL;
END;
$$;
CREATE TRIGGER quota_seats AFTER INSERT OR UPDATE OR DELETE ON identity.memberships
    FOR EACH ROW EXECUTE FUNCTION identity.count_quota_seat();
SELECT platform.record_quota_counter(tenant_id,'seats',DATE '1970-01-01',count(*))
  FROM identity.memberships WHERE status='active' GROUP BY tenant_id;
