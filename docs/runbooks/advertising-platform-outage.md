# Runbook — advertising platform outage / degraded mode

**Alerts:** `AdsSyncFailuresSpike`, `ConversionUploadFailureSpike`, `AdsSyncDurationHigh`
(see [advertising-alerts](./advertising-alerts.md)).

An upstream API outage or severe throttling event at Google Ads or Meta.

## Architecture and degradation behavior

All outbound platform traffic routes through `RateLimitedAdapter`
(`crates/contexts/advertising/src/adapters/rate_limit.rs`), which enforces a two-tier
`QuotaLimiter`:
- **Platform-wide quota**: `quota_platform_wide_permits = 20` (configured in
  `configs/base.toml` under `[advertising]`), protecting shared developer-token and
  app-level rate limits across all tenants.
- **Per-connection quota**: `quota_per_connection_permits = 3`, preventing a single
  active tenant or heavy job from starving other tenants.

The four primary functional paths degrade independently:

| Path | Behavior during outage | Impact & Recovery |
|---|---|---|
| **Connect** (OAuth start & callback) | `StartAuthorization` and `HandleCallback` fail fast with `PlatformUnavailable` / timeout. | Tenant sees a clear error prompt in the UI. No corrupt or half-formed `Active` connection row is created. |
| **Publish** (Campaign CRUD & mutations) | Mutation calls (`PublishCampaign`, `PauseCampaign`, `ResumeCampaign`, `PatchCampaign`) queue on semaphores; timeouts return `502 Bad Gateway` (`PlatformUnavailable`). | Tenant campaign intent snapshots remain safely stored. Optimistic concurrency (`expected_revision`) returns `409 Conflict` on race conditions, preventing out-of-order writes. |
| **Ingestion** (Metrics rollups) | `RunMetricsIngestion` worker sweep catches upstream HTTP 429/5xx, increments `ads_sync_failures_total{platform, code}`, logs warning, and leaves `last_metrics_ingested_at` untouched. | Gauge `metrics_ingestion_lag_hours` rises. When the platform recovers, the replace-window sweep (`backfill_days = 7`) automatically heals all missing history without partial row corruption. See [metrics ingestion backlog](./advertising-ingestion-backlog.md). |
| **Upload** (Conversions & Audiences) | `UploadPendingConversions` increments attempt counts per failed event. Events reaching 5 failed attempts transition to `UploadState::Parked` (DLQ). `SyncAudiences` fails the pass, retaining current membership. | Conversions remain securely buffered in `advertising.conversion_events`. No data is dropped. Once the platform recovers, parked events are retried. See [upload failure surge](./advertising-upload-failure-surge.md). |

## 1. Confirm & Triage

1. Check external status dashboards:
   - Google Ads API Status Dashboard
   - Meta Graph API / Marketing API Status Dashboard
2. Check Prometheus metrics for error concentration:
   ```promql
   sum(rate(ads_sync_failures_total[10m])) by (platform, code)
   sum(rate(conversion_upload_total{result="failed"}[10m])) by (platform)
   ```
3. Inspect recent sync errors in database:
   ```sql
   SELECT platform,
          count(*) AS failing_connections,
          jsonb_agg(DISTINCT last_error->>'code') AS error_codes,
          max(updated_at) AS last_failure_at
   FROM advertising.connections
   WHERE last_error IS NOT NULL AND updated_at > now() - interval '2 hours'
   GROUP BY platform;
   ```

## 2. During the incident

- **Do not** toggle feature flags or drop database tables. The adapter fail-safe
  queues and worker intervals are designed to absorb outages.
- **Do not** modify token secrets or developer tokens unless advised by upstream
  security notices.
- Monitor `conversion_dlq_depth` to observe the rate of parked conversions.

## 3. Post-outage recovery

Once the upstream platform reports recovery:

1. Ingestion auto-heals: `RunMetricsIngestion` will re-fetch up to `backfill_days = 7`
   days of metrics on the next hourly schedule, atomically replacing the window via
   `SqlxMetricsRepository::replace_window`.
2. Inspect remaining DLQ events:
   ```sql
   SELECT count(*) FROM advertising.conversion_events, jsonb_each(upload_states) s WHERE s.value->>'status' = 'parked';
   ```
   If parked conversions accumulated, trigger manual retries following
   [conversion upload failure surge](./advertising-upload-failure-surge.md).
