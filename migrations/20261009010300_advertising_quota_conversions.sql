-- Persisted conversion ingestion counts; retention deletion never refunds it.
CREATE FUNCTION advertising.count_quota_conversion() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM platform.record_quota_counter(NEW.tenant_id,'tracked_conversions',
      date_trunc('month',NEW.created_at AT TIME ZONE 'UTC')::date,1);
    RETURN NULL;
END;
$$;
CREATE TRIGGER quota_conversions AFTER INSERT ON advertising.conversion_events
    FOR EACH ROW EXECUTE FUNCTION advertising.count_quota_conversion();
SELECT platform.record_quota_counter(tenant_id,'tracked_conversions',
    date_trunc('month',created_at AT TIME ZONE 'UTC')::date,count(*))
  FROM advertising.conversion_events
  WHERE created_at >= date_trunc('month',now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
  GROUP BY tenant_id,date_trunc('month',created_at AT TIME ZONE 'UTC')::date;
