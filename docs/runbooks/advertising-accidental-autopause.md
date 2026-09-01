# Runbook — accidental campaign auto-pause & budget guardrail recovery

**Alerts:** `BudgetAutoPauseSpike`, `BudgetThresholdBreached` (see [advertising-alerts](./advertising-alerts.md)).

An active campaign has been automatically paused by the budget guardrails system
due to a cap breach or provisional spend threshold, but the tenant wishes to resume
it immediately or adjust the cap configuration.

## Background & Guardrail behavior

The budget guardrail evaluator (`crates/contexts/advertising/src/application/budget/evaluator.rs`)
runs every `advertising_budget_guardrails_interval_secs = 300`:
- **Threshold evaluation**: If spend reaches 80% or 100% of a configured `BudgetCap`
  (`advertising.budget_caps`), an alert is emitted to `advertising.budget_alerts`
  and localized notifications are dispatched.
- **Auto-pause execution**: If `auto_pause = true` is enabled on the cap and spend
  hits 100% (or the confirmed provisional lower bound exceeds the cap),
  `ThresholdEvaluator::execute_auto_pause` pauses the campaign with attribution:
  `paused_by: "system:budget_guardrail"`.
- **Audit tracking**: The pause action records an audit record in
  `advertising.campaign_changes` with `attribution = 'system:budget_guardrail'`
  (`actor_id` stays `NULL` — that column is a tenant-user FK).
- **Rollover sweep**: At the start of a new budget period (daily or monthly),
  `BudgetRolloverSweep` (`crates/contexts/advertising/src/application/budget/rollover.rs`)
  automatically resumes the campaign **only if** `auto_resume_on_rollover = true` was
  configured on the cap.

## 1. Identify the auto-paused campaign

Query recent guardrail-triggered alerts:

```sql
SELECT a.id AS alert_id, a.tenant_id, a.campaign_id, a.cap_id,
       a.condition, a.threshold, a.spend_minor,
       a.cap_minor, a.currency, a.created_at,
       c.name AS campaign_name, c.status AS campaign_status
FROM advertising.budget_alerts a
JOIN advertising.campaigns c ON c.id = a.campaign_id
WHERE a.condition IN ('settled_breach', 'provisional_lower_bound_breach')
  AND a.auto_paused
  AND a.created_at > now() - interval '24 hours'
ORDER BY a.created_at DESC;
```

Check the campaign mutation audit log — the system attribution lives in the
`attribution` text column added alongside `metadata` by TASK-017
(`actor_id` is a bigint FK and stays `NULL` for guardrail-triggered pauses):

```sql
SELECT id, campaign_id, actor_id, attribution, source,
       before_state->>'status' AS before_status,
       after_state->>'status' AS after_status, metadata, created_at
FROM advertising.campaign_changes
WHERE attribution = 'system:budget_guardrail'
  AND created_at > now() - interval '24 hours'
ORDER BY created_at DESC;
```

## 2. Evaluate cause & Options

1. **Unsettled / Provisional Data Spike**:
   If the auto-pause triggered due to unsettled conversion attributions or delayed
   platform reporting, check if `DataFreshness::is_settled` was false at trigger time.
2. **Intentional vs Accidental Cap**:
   Check if the budget cap amount was set too low relative to normal campaign volume:
   ```sql
   SELECT id, tenant_id, campaign_id, period, amount_minor, currency,
          auto_pause, auto_resume_on_rollover, created_at
   FROM advertising.budget_caps
   WHERE tenant_id = <tenant_id> AND (campaign_id = <campaign_id> OR campaign_id IS NULL);
   ```

## 3. Resume campaign & Adjust budget caps

### Step 1: Update or disable the budget cap (if desired)

To raise the cap or disable auto-pause:

```bash
# Update campaign-level cap:
curl -X PUT -H "Authorization: Bearer $TENANT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "amount": {"amount_minor": 100000, "currency": "USD"},
    "period": "daily",
    "auto_pause": false,
    "auto_resume_on_rollover": true
  }' \
  https://api.sanvi.internal/api/v1/tenant/ads/campaigns/<campaign_id>/budget-cap
```

### Step 2: Acknowledge the breach alert

```bash
curl -X POST -H "Authorization: Bearer $TENANT_TOKEN" \
  https://api.sanvi.internal/api/v1/tenant/ads/budget-alerts/<alert_id>/acknowledge
```

### Step 3: Manually resume the campaign

Execute `ResumeCampaign` (`crates/contexts/advertising/src/application/campaigns.rs`):

```bash
curl -X POST -H "Authorization: Bearer $TENANT_TOKEN" \
  -H "Idempotency-Key: $(uuidgen)" \
  https://api.sanvi.internal/api/v1/tenant/ads/campaigns/<campaign_id>/resume
```

This updates `advertising.campaigns` status to `'active'`, records a campaign change
mutation under the tenant's user ID, and sends an unpause request to the ad platform
adapter.

## 4. Verify

1. Verify the campaign status is active in PostgreSQL:
   ```sql
   SELECT id, name, status, updated_at FROM advertising.campaigns WHERE id = <campaign_id>;
   ```
2. Verify in the Google Ads or Meta Ads manager that the campaign delivery status is Active / Running.
