# Advertising alert rules (phase 10)

**Alerts:** `AdvertisingConsentMissing` — the invariant alarm below is this file's own runbook.

The metric set every alert reads lives in `crates/contexts/advertising/src/adapters/metrics.rs`
(`conversion_*`, `ads_sync_*`, `metrics_ingestion_*`, `budget_*`, exposed on `/metrics`).
This file is the canonical list of phase-10 alerts: name, condition, severity, and the
runbook that tells you what to do when one fires. Wire them into the monitoring stack
verbatim; a rule here without a runbook section is a bug.

## The invariant alarm

| Alert | Condition | Severity |
|---|---|---|
| `AdvertisingConsentMissing` | `increase(conversion_suppressed_total{reason="missing_consent"}[5m]) > 0` or any conversion row where `consent IS NULL` | **page immediately** |

Sanvi never transmits advertising conversion events without an explicit consent
snapshot: the `advertising.conversion_events.consent` column is `NOT NULL` at
the PostgreSQL schema level (`migrations/20260826000000_advertising_core.sql`),
and CI enforces via `scripts/check-ads-invariants.sh` that every adapter upload port
requires a `ConsentSnapshot` and `HashedIds`. Any attempt to persist or upload
an advertising conversion without consent is impossible by construction. Treat
any firing as a sev-1: immediately halt conversion upload sweeps, inspect the
capturing request origin, and verify schema integrity.

## Conversion & Audience transport

| Alert | Condition | Severity | Runbook |
|---|---|---|---|
| `ConversionUploadFailureSpike` | failure ratio `sum(rate(conversion_upload_total{result="failed"}[10m])) / sum(rate(conversion_upload_total[10m])) > 0.05` for > 15m (upload failure ratio > 5%) | page if sustained | [conversion upload failure surge](./advertising-upload-failure-surge.md) |
| `ConversionDeadLetterDepth` | `conversion_dlq_depth > 0` (events parked after 5 failed attempts; `SELECT count(*) FROM advertising.conversion_events, jsonb_each(upload_states) s WHERE s.value->>'status' = 'parked'`) | page (parked events in DLQ) | [conversion upload failure surge](./advertising-upload-failure-surge.md) |
| `ConversionUploadLagHigh` | p95 of `conversion_upload_lag_seconds` > 300s for > 15m | ticket | [conversion upload failure surge](./advertising-upload-failure-surge.md) |
| `ConversionRetractionUnpropagatedSpike` | `increase(conversion_retraction_total{result="unpropagated"}[1h]) > 0` | ticket | [audience & conversion retraction](./advertising-audience-retraction.md) |
| `AudienceSyncFailure` | `increase(audience_sync_total{result="failed"}[1h]) > 0` | ticket | [audience & conversion retraction](./advertising-audience-retraction.md) |

## Metrics ingestion & Rollups

| Alert | Condition | Severity | Runbook |
|---|---|---|---|
| `MetricsIngestionLagHigh` | `metrics_ingestion_lag_hours > 6` for > 1h (worst observed lag across active connections > 6h) | page (reporting data stale) | [metrics ingestion backlog](./advertising-ingestion-backlog.md) |
| `AdsSyncFailuresSpike` | `sum(rate(ads_sync_failures_total[15m])) > 0` | ticket | [metrics ingestion backlog](./advertising-ingestion-backlog.md) |
| `AdsSyncDurationHigh` | p95 of `ads_sync_duration_seconds` > 30s for > 30m | ticket | [platform outage / degraded mode](./advertising-platform-outage.md) |

## Budget & Money

| Alert | Condition | Severity | Runbook |
|---|---|---|---|
| `AdvertisingSpendAnomaly` | `SpendAnomalyDetector` flags spend > 3-sigma above 14-day trailing mean for campaign (`SELECT id FROM advertising.budget_alerts WHERE condition = 'spend_anomaly' AND created_at > now() - interval '1h'`) | ticket per tenant/campaign | [spend anomaly](./advertising-spend-anomaly.md) |
| `BudgetThresholdBreached` | `increase(budget_threshold_alerts_total{threshold="100"}[5m]) > 0` | info (tenant notification) | [accidental auto-pause](./advertising-accidental-autopause.md) |
| `BudgetAutoPauseSpike` | `increase(budget_auto_pause_total[15m]) > 0` (campaigns auto-paused with `system:budget_guardrail` attribution) | ticket | [accidental auto-pause](./advertising-accidental-autopause.md) |

## Connection lifecycle & Infrastructure

| Alert | Condition | Severity | Runbook |
|---|---|---|---|
| `AdConnectionRevoked` | Connection transitioned to `disconnected`/`expired` (`SELECT count(*) FROM advertising.connections WHERE status IN ('disconnected', 'expired')`) — see the runbook's "Known gap" for why `disconnected_notified_at` can't yet be used to dedupe this alert | ticket per tenant | [token revoked](./advertising-token-revoked.md) |
| `PartitionMaintenanceFailed` | Missing future partitions for `conversion_events` or `metrics_daily` within 7 days of month start (`SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'advertising' AND c.relname = format('conversion_events_%s', to_char(now() + interval '1 month', 'YYYYMM'))`) = 0 | page | [partition maintenance failure](./advertising-partition-maintenance-failure.md) |

## Notes

- Gauges (`conversion_dlq_depth`, `metrics_ingestion_lag_hours`) are updated on each worker sweep pass.
- Counters (`conversion_upload_total`, `conversion_suppressed_total`, `conversion_retraction_total`, `audience_sync_total`, `ads_sync_failures_total`, `budget_threshold_alerts_total`, `budget_auto_pause_total`) track domain operations with explicit result labels.
- The `MetricsIngestionLagHigh` alert derives its value from each connection's post-sweep state, falling back to `connected_at` for connections that have never completed an ingestion pass.

## Known gaps

- **No scheduled token-refresh sweep**: Currently, no scheduled token-refresh worker sweep is registered in `apps/worker/src/main.rs`. While the domain transition logic (`refresh_failure_transition` in `crates/contexts/advertising/src/domain/connection_lifecycle.rs` and `record_permanent_refresh_failure` in `crates/contexts/advertising/src/domain/connection.rs`) exists and is tested, there is no active worker loop emitting `ads_token_refresh_failures` (a metric required by TASK-018's specification). This should be addressed in a TASK-011 follow-up rather than creating an unbacked alert rule.
