# Runbook — advertising spend anomaly (>3 sigma)

**Alert:** `AdvertisingSpendAnomaly` (see [advertising-alerts](./advertising-alerts.md)).

A campaign's spend for today has spiked more than 3 standard deviations above its
14-day trailing historical mean, indicating potential runaway bidding, misconfigured
targeting, click fraud, or unexpected traffic bursts.

## Detection mechanism

Spend anomalies are evaluated during the budget guardrails sweep
(`advertising_budget_guardrails_interval_secs = 300`) by `SpendAnomalyDetector`
(`crates/contexts/advertising/src/application/budget/evaluator.rs`):
1. Runs a `MetricsRangeQuery` against `advertising.metrics_daily` over a trailing
   14-day window (`today - 14 days` to `today`).
2. Collects historical days where spend was recorded (requires at least 3 historical
   sample days and `today_spend > 0`).
3. Computes the historical mean and standard deviation $\sigma$.
4. When `sigma_multiple = (today_spend - mean) / std_dev > 3.0`, it emits a
   structured log (`tracing::warn!`, message `"Spend anomaly (>3sigma) detected
   for campaign"`) and calls `BudgetNotifier::notify_spend_anomaly`.

**Known gap:** unlike `ThresholdEvaluator`'s 80%/100% breaches (which call
`BudgetAlertRepository::record_alert` — see `crates/contexts/advertising/src/adapters/budget_repo.rs`),
`SpendAnomalyDetector` never writes a row to `advertising.budget_alerts`.
`AlertCondition::SpendAnomaly`/`'spend_anomaly'` exists as a schema value and
round-trips through `AlertCondition::parse`/`as_str`, but nothing constructs
it — so a query against `budget_alerts WHERE condition = 'spend_anomaly'`
always returns zero rows. The only wired implementation of `BudgetNotifier`
is `LoggingBudgetNotifier` (`crates/contexts/advertising/src/adapters/notifications.rs`),
which just emits a second `tracing::warn!` — there is no persisted alert row,
tenant-facing API surface, or actual delivered notification for this
condition today. Raise this as a follow-up rather than querying for a row
that can't exist; until then, this alert is log-only.

## 1. Identify anomalous campaign and spend figures

Search structured logs for the detector's warning (adjust to your log
aggregator):

```
message:"Spend anomaly (>3sigma) detected for campaign" AND timestamp:[now-24h TO now]
```

The log fields carry `tenant_id`, `campaign_id`, `today_spend`, `mean`, and
`sigma_multiple` directly — no database join is needed to identify the
campaign.

Inspect the campaign's trailing 14-day daily spend and metrics:

```sql
SELECT date, platform, spend_minor, currency, impressions, clicks,
       conversions, conversion_value_minor, sanvi_revenue_minor
FROM advertising.metrics_daily
WHERE tenant_id = <tenant_id> AND campaign_id = <campaign_id>
  AND date >= current_date - interval '14 days'
ORDER BY date DESC;
```

## 2. Triage & Investigation

1. **Verify ad platform state**:
   Check whether the spike corresponds to a recent budget or bid increase on Google Ads
   or Meta.
2. **Review campaign audit logs**:
   Query `advertising.campaign_changes` to verify if a tenant user modified the
   budget or bidding strategy:
   ```sql
   SELECT id, actor_id, source, changed_fields,
          before_state->'budget' AS before_budget,
          after_state->'budget' AS after_budget, created_at
   FROM advertising.campaign_changes
   WHERE tenant_id = <tenant_id> AND campaign_id = <campaign_id>
   ORDER BY created_at DESC LIMIT 10;
   ```
3. **Compare spend vs revenue**:
   Check if `sanvi_revenue_minor` or conversion count grew proportionally. If clicks
   and spend surged with 0 conversions, suspect invalid traffic / click fraud.

## 3. Mitigation

- **Legitimate surge (e.g. flash sale / viral event)**:
  No action needed — there is no persisted alert row for `spend_anomaly` to
  acknowledge (see the known gap above). `POST .../budget-alerts/{alert_id}/acknowledge`
  only applies to a genuine 80%/100% threshold breach row.
- **Runaway or accidental spend**:
  1. Immediately pause the campaign:
     ```bash
     curl -X POST -H "Authorization: Bearer $TENANT_TOKEN" \
       -H "Idempotency-Key: $(uuidgen)" \
       https://api.sanvi.internal/api/v1/tenant/ads/campaigns/<campaign_id>/pause
     ```
  2. Apply or lower a hard budget cap with `auto_pause = true`:
     ```bash
     curl -X PUT -H "Authorization: Bearer $TENANT_TOKEN" \
       -H "Content-Type: application/json" \
       -d '{"amount": {"amount_minor": 50000, "currency": "USD"}, "period": "daily", "auto_pause": true}' \
       https://api.sanvi.internal/api/v1/tenant/ads/campaigns/<campaign_id>/budget-cap
     ```

## 4. Verify

1. Confirm campaign status is updated:
   ```sql
   SELECT id, name, status, updated_at FROM advertising.campaigns WHERE id = <campaign_id>;
   ```
2. Check `GET /api/v1/tenant/ads/budget-alerts` to ensure the alert shows `acknowledged_at` populated.
