# Runbook — advertising metrics ingestion backlog & restatement recovery

**Alerts:** `MetricsIngestionLagHigh`, `AdsSyncFailuresSpike` (see [advertising-alerts](./advertising-alerts.md)).

Metrics ingestion lag for active advertising connections has exceeded 6 hours,
causing stale campaign performance dashboards, delayed ROAS reporting, and delayed
budget guardrail evaluations.

## Background & Ingestion architecture

Metrics synchronization runs on the worker via `RunMetricsIngestion`
(`crates/contexts/advertising/src/application/metrics/sweep.rs`) on a unified schedule
(`advertising_metrics_ingestion_interval_secs = 3600`):
- **Due connection batching**: Fetches connections whose `last_metrics_ingested_at`
  is older than 1 hour or `NULL`.
- **Atomic window replacement**: For each connection, `IngestConnectionMetrics`
  replaces the entire backfill window (`[today - backfill_days + 1, today]`,
  default 7 days) using `SqlxMetricsRepository::replace_window` (delete-then-insert).
  This guarantees that ad platform restatements or removed campaign days cleanly delete
  stale local records rather than leaving phantom spend.
- **Deterministic revenue join**: `JoinCampaignRevenue` joins conversion events to
  campaign days. If revenue cannot be uniquely assigned to a single active campaign on
  that platform, it is safely parked in `advertising.metrics_unattributed_revenue_daily`
  rather than split on an arbitrary guess.
- **Observability**: Connection cursor `last_metrics_ingested_at` updates only upon
  successful ingestion; `metrics_ingestion_lag_hours` reflects the true worst-case lag.

## 1. Identify lagging connections and error causes

Query active connections with high ingestion lag:

```sql
SELECT id, tenant_id, platform, external_account_id, account_name,
       last_metrics_ingested_at,
       round(EXTRACT(EPOCH FROM (now() - COALESCE(last_metrics_ingested_at, connected_at))) / 3600, 2) AS lag_hours,
       last_error
FROM advertising.connections
WHERE status = 'active'
ORDER BY lag_hours DESC
LIMIT 25;
```

Check the freshness endpoint output:

```bash
curl -s -H "Authorization: Bearer $TENANT_TOKEN" \
  https://api.sanvi.internal/api/v1/tenant/ads/metrics/freshness | jq .
```

Inspect unattributed revenue parked during recent syncs:

```sql
SELECT tenant_id, platform, date, revenue_minor, currency, updated_at
FROM advertising.metrics_unattributed_revenue_daily
WHERE date >= current_date - interval '7 days'
ORDER BY date DESC, tenant_id;
```

## 2. Diagnose failure modes

1. **Platform API Outage / Rate Limit**:
   If `ads_sync_failures_total` is spiking with 429 or 5xx codes from Google Ads or Meta,
   the `QuotaLimiter` may be saturated. Follow
   [advertising platform outage](./advertising-platform-outage.md).
2. **Invalid / Revoked Credentials**:
   If `last_error` indicates authentication failure (`unauthenticated`, `invalid_grant`),
   treat it as a revocation — `status` will **not** flip to `disconnected` on its
   own (see the known gap in [token revoked](./advertising-token-revoked.md)).
3. **Worker Scheduler Blocked / Crashed**:
   Check worker process logs:
   ```bash
   docker logs sanvi-worker --tail 200 | grep "advertising-metrics-ingestion"
   ```

## 3. Recovery & Restatement

No manual SQL backfilling is required. The ingestion design handles restatement
automatically:
1. When connectivity is restored, the next run of `RunMetricsIngestion` selects all
   lagging connections.
2. It fetches the full 7-day restatement window from the ad platforms.
3. `replace_window` deletes and re-inserts all daily metric rows for the period,
   restoring full parity with upstream platforms.
4. `JoinCampaignRevenue` re-evaluates conversion attributions across the window.

## 4. Verify

1. Confirm `metrics_ingestion_lag_hours` gauge drops below 1.0 hour.
2. Verify daily metrics rows exist for today and trailing days:
   ```sql
   SELECT platform, date, count(*) AS campaign_count,
          sum(spend_minor) AS total_spend, sum(sanvi_revenue_minor) AS total_revenue
   FROM advertising.metrics_daily
   WHERE date >= current_date - interval '3 days'
   GROUP BY platform, date
   ORDER BY date DESC, platform;
   ```
3. Confirm `GET /api/v1/tenant/ads/metrics/freshness` reports `is_settled: true` and `is_stale: false`.
