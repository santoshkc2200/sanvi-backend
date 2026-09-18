//! Budget threshold evaluation, provisional data guard, auto-pause execution,
//! and spend anomaly detection (TASK-017, slice 10.8).

use std::sync::Arc;

use chrono::Utc;
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::adapters::AdvertisingMetrics;
use crate::domain::model::{
    AlertCondition, BudgetAlert, BudgetCap, BudgetPeriod, CampaignId, CampaignStatus,
    DataFreshness, Money,
};
use crate::domain::{
    AdEvent, AdvertisingError, BudgetAlertRepository, BudgetCapRepository, CampaignRepository,
    EventPublisher, IdGenerator, MetricsGroupBy, MetricsRangeQuery, MetricsRepository,
    StoredCampaign,
};

pub struct ThresholdEvaluator {
    tx: TxManager,
    caps: Arc<dyn BudgetCapRepository>,
    alerts: Arc<dyn BudgetAlertRepository>,
    campaigns: Arc<dyn CampaignRepository>,
    pause_campaign: Arc<crate::application::campaigns::PauseCampaign>,
    status_query: Arc<super::status::GetSpendStatus>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    metrics: Arc<AdvertisingMetrics>,
}

impl ThresholdEvaluator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx: TxManager,
        caps: Arc<dyn BudgetCapRepository>,
        alerts: Arc<dyn BudgetAlertRepository>,
        campaigns: Arc<dyn CampaignRepository>,
        pause_campaign: Arc<crate::application::campaigns::PauseCampaign>,
        status_query: Arc<super::status::GetSpendStatus>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        metrics: Arc<AdvertisingMetrics>,
    ) -> Self {
        Self {
            tx,
            caps,
            alerts,
            campaigns,
            pause_campaign,
            status_query,
            events,
            ids,
            metrics,
        }
    }

    /// Read every active cap, in a short transaction that is committed before
    /// returning.
    ///
    /// Evaluation deliberately does *not* run under this transaction: each
    /// cap's spend read and alert writes open their own short transactions,
    /// so holding one across the whole sweep would keep a platform
    /// transaction open over hundreds of nested acquisitions and can exhaust
    /// the pool.
    pub async fn list_active_caps(&self) -> Result<Vec<BudgetCap>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_unscoped("advertising_budget_guardrails", "system")
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let caps = self.caps.list_all_active_caps(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(caps)
    }

    /// Evaluate the given caps. Caps are grouped by tenant and each tenant's
    /// context — connections, campaigns, timezone — is loaded once per sweep
    /// and shared by every cap: loading it per cap (and again inside the
    /// spend calculation) made a sweep over C caps of one tenant transfer
    /// O(C²) campaign rows. Each cap still opens its own short transactions
    /// for the metrics read and any alert writes. A failing cap is logged
    /// and counted, and the sweep continues.
    pub async fn evaluate_caps(&self, caps: &[BudgetCap]) -> Result<(), AdvertisingError> {
        let mut groups: std::collections::BTreeMap<TenantId, Vec<&BudgetCap>> =
            std::collections::BTreeMap::new();
        for cap in caps {
            groups.entry(cap.tenant_id).or_default().push(cap);
        }

        for (tenant_id, group) in groups {
            let context = match self.status_query.load_spend_context(tenant_id).await {
                Ok(context) => context,
                Err(e) => {
                    tracing::warn!(
                        %tenant_id,
                        cap_count = group.len(),
                        error = %e,
                        "Budget cap sweep: tenant context load failed; skipping tenant's caps"
                    );
                    self.metrics
                        .budget_cap_eval_failures_total()
                        .with_label_values(&["tenant", e.error_kind()])
                        .inc();
                    continue;
                }
            };
            for cap in group {
                self.evaluate_counted(tenant_id, cap, &context).await;
            }
        }
        Ok(())
    }

    #[tracing::instrument(name = "advertising.evaluate_tenant", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn evaluate_tenant(&self, tenant_id: TenantId) -> Result<(), AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let caps = self.caps.list_by_tenant(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let context = self.status_query.load_spend_context(tenant_id).await?;
        for cap in &caps {
            self.evaluate_counted(tenant_id, cap, &context).await;
        }
        Ok(())
    }

    /// Evaluate one cap in isolation, loading the tenant context for it.
    /// Caps saved or mutated through the admin API land here; scheduled
    /// sweeps share one context across a tenant's caps via
    /// [`evaluate_caps`](Self::evaluate_caps) / [`evaluate_tenant`](Self::evaluate_tenant).
    pub async fn evaluate_single_cap(
        &self,
        tenant_id: TenantId,
        cap: &BudgetCap,
    ) -> Result<(), AdvertisingError> {
        let context = self.status_query.load_spend_context(tenant_id).await?;
        self.evaluate_cap_in_context(tenant_id, cap, &context).await
    }

    /// Evaluate one cap against a preloaded tenant context, logging and
    /// counting a failure instead of aborting the sweep.
    async fn evaluate_counted(
        &self,
        tenant_id: TenantId,
        cap: &BudgetCap,
        context: &super::status::SpendContext,
    ) {
        if let Err(e) = self.evaluate_cap_in_context(tenant_id, cap, context).await {
            let scope_label = cap
                .campaign_id
                .map(|cid| format!("campaign:{cid}"))
                .unwrap_or_else(|| "tenant".to_string());
            tracing::warn!(
                tenant_id = %tenant_id,
                cap_id = %cap.id,
                scope = %scope_label,
                error = %e,
                "Budget cap evaluation failed; skipping cap and continuing sweep"
            );
            self.metrics
                .budget_cap_eval_failures_total()
                .with_label_values(&[&scope_label, e.error_kind()])
                .inc();
        }
    }

    async fn evaluate_cap_in_context(
        &self,
        tenant_id: TenantId,
        cap: &BudgetCap,
        context: &super::status::SpendContext,
    ) -> Result<(), AdvertisingError> {
        let now = Utc::now();

        // The period key must be "today" in the tenant's timezone — the same
        // window `calculate_from_context` sums on this shared context — or a
        // cap breached in JST dedupes against (or misses) a UTC-keyed alert
        // row.
        let today = context.now.date_naive();
        let period_start = cap.period.period_start(today);
        // Freshness comes from the metrics-ingestion cursor of the connections
        // feeding this cap's scope — never the OAuth `last_synced_at`, which a
        // token refresh would move without any new spend data arriving, and
        // never the tenant-wide aggregate for a campaign-scoped cap, where an
        // unrelated stale connection would suppress a valid pause (or a fresh
        // unrelated one would bless stale spend).
        let freshness = super::status::freshness_for_scope(
            &context.connections,
            &context.campaigns,
            cap.campaign_id,
        );
        let is_stale = freshness.is_stale;

        // Calculate spend details
        let spend_details = self
            .status_query
            .calculate_from_context(
                context,
                tenant_id,
                cap.campaign_id,
                cap.period,
                &cap.amount.currency,
                cap.declared_fx_basis.as_deref(),
                cap.fx_rate_date,
            )
            .await?;

        let total_spend = spend_details.total.clone();
        let has_unconvertible = !spend_details.unconvertible_currencies.is_empty();
        if has_unconvertible {
            tracing::warn!(
                %tenant_id,
                cap_id = %cap.id,
                unconvertible = ?spend_details.unconvertible_currencies,
                // With a declared basis the gap is rate coverage and waiting
                // can fix it; without one, the cap itself must be re-put
                // with a basis or a matching currency — waiting changes
                // nothing.
                "Budget cap evaluation skipped: spend data in unconvertible currencies; \
                 treating as stale data — {}",
                if cap.declared_fx_basis.is_some() {
                    "cap will not be enforced until FX coverage is complete"
                } else {
                    "cap will not be enforced until it declares an FX basis or \
                     uses a cap currency matching the spend"
                }
            );
            // Treat like stale data: return Ok so the sweep continues to other caps.
            return Ok(());
        }
        let cap_minor = cap.amount.amount_minor;
        let scope_str = match cap.campaign_id {
            Some(cid) => format!("campaign:{cid}"),
            None => "tenant".to_string(),
        };
        let scope_metric_label = match cap.campaign_id {
            Some(_) => "campaign",
            None => "tenant",
        };

        // 1. Evaluate 80% threshold
        let threshold_80_amount = (cap_minor * 80) / 100;
        if total_spend.amount_minor >= threshold_80_amount {
            let mut scope = self
                .tx
                .begin_for(tenant_id.as_i64())
                .await
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

            let already_fired = self
                .alerts
                .has_alert_fired(&mut scope, tenant_id, cap.id, period_start, 80)
                .await?;

            if !already_fired {
                let alert_id = self.ids.next_budget_alert_id()?;
                let alert = BudgetAlert {
                    id: alert_id,
                    tenant_id,
                    cap_id: Some(cap.id),
                    campaign_id: cap.campaign_id,
                    period: cap.period,
                    period_start,
                    threshold: 80,
                    condition: AlertCondition::Threshold80,
                    spend: total_spend.clone(),
                    cap_amount: cap.amount.clone(),
                    data_freshness: freshness.clone(),
                    auto_paused: false,
                    acknowledged_at: None,
                    acknowledged_by: None,
                    created_at: now,
                };

                self.alerts.record_alert(&mut scope, &alert).await?;
                self.events
                    .publish_scoped(
                        &mut scope,
                        tenant_id,
                        &[AdEvent::SpendThresholdReached {
                            campaign_id: cap.campaign_id,
                            threshold: 80,
                            alert_id: Some(alert.id),
                            scope_name: Some(scope_str.clone()),
                            spend_minor: Some(total_spend.amount_minor),
                            currency: Some(total_spend.currency.to_string()),
                            cap_minor: Some(cap.amount.amount_minor),
                            period: Some(cap.period),
                            action_taken: None,
                            condition: Some(AlertCondition::Threshold80),
                        }],
                    )
                    .await?;
                scope
                    .commit()
                    .await
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

                self.metrics
                    .budget_threshold_alerts_total()
                    .with_label_values(&[scope_metric_label, "80"])
                    .inc();
            } else {
                scope
                    .commit()
                    .await
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
            }
        }

        // 2. Evaluate 100% threshold
        if total_spend.amount_minor >= cap_minor {
            let mut scope = self
                .tx
                .begin_for(tenant_id.as_i64())
                .await
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

            let already_fired = self
                .alerts
                .has_alert_fired(&mut scope, tenant_id, cap.id, period_start, 100)
                .await?;

            // Finding 1: ALWAYS evaluate whether a pause should be triggered, even if an alert
            // was previously recorded (e.g. StaleDataWarning). A prior StaleDataWarning must NOT
            // block a SettledBreach auto-pause when data becomes settled and the cap is exceeded.
            // Only suppress duplicate *notifications and alert records*; never suppress the *pause*.
            let mut auto_paused = false;
            let (condition, action_taken_desc) = if is_stale {
                (
                    AlertCondition::StaleDataWarning,
                    "Guardrails will not pause on unconfirmed data.",
                )
            } else if spend_details.settled.amount_minor >= cap_minor {
                if cap.auto_pause {
                    let (paused_count, pause_failures, already_paused) = self
                        .execute_auto_pause(
                            tenant_id,
                            cap,
                            &total_spend,
                            &freshness,
                            "settled_breach",
                        )
                        .await;
                    if !pause_failures.is_empty() {
                        tracing::error!(
                            %tenant_id,
                            cap_id = %cap.id,
                            failure_count = pause_failures.len(),
                            "Budget guardrail: some auto-pauses failed for settled_breach"
                        );
                    }
                    auto_paused = paused_count > 0;
                    self.metrics
                        .budget_auto_pause_total()
                        .with_label_values(&["settled_breach"])
                        .inc_by(paused_count as u64);
                    (
                        AlertCondition::SettledBreach,
                        if paused_count > 0 {
                            "Campaign(s) auto-paused by budget guardrail."
                        } else if pause_failures.is_empty() && already_paused > 0 {
                            "Campaign(s) already paused or inactive; guardrail pause skipped."
                        } else {
                            "Auto-pause attempted but all pauses failed."
                        },
                    )
                } else {
                    (
                        AlertCondition::SettledBreach,
                        "No automatic pause configured.",
                    )
                }
            } else if spend_details.provisional_lower_bound.amount_minor >= cap_minor {
                if cap.auto_pause {
                    let (paused_count, pause_failures, already_paused) = self
                        .execute_auto_pause(
                            tenant_id,
                            cap,
                            &total_spend,
                            &freshness,
                            "provisional_lower_bound_breach",
                        )
                        .await;
                    if !pause_failures.is_empty() {
                        tracing::error!(
                            %tenant_id,
                            cap_id = %cap.id,
                            failure_count = pause_failures.len(),
                            "Budget guardrail: some auto-pauses failed for provisional_lower_bound_breach"
                        );
                    }
                    auto_paused = paused_count > 0;
                    self.metrics
                        .budget_auto_pause_total()
                        .with_label_values(&["provisional_lower_bound_breach"])
                        .inc_by(paused_count as u64);
                    (
                        AlertCondition::ProvisionalLowerBoundBreach,
                        if paused_count > 0 {
                            "Campaign(s) auto-paused on confirmed provisional lower bound breach."
                        } else if pause_failures.is_empty() && already_paused > 0 {
                            "Campaign(s) already paused or inactive; guardrail pause skipped."
                        } else {
                            "Auto-pause attempted but all pauses failed."
                        },
                    )
                } else {
                    (
                        AlertCondition::ProvisionalLowerBoundBreach,
                        "No automatic pause configured.",
                    )
                }
            } else {
                // Provisional total >= cap, but lower bound < cap
                (
                    AlertCondition::ProvisionalWarning,
                    "Provisional spend is above cap but lower bound is below. Guardrails will not pause on provisional data.",
                )
            };

            if !already_fired {
                // First time this cap/period/threshold has fired: record alert + event.
                let alert_id = self.ids.next_budget_alert_id()?;
                let alert = BudgetAlert {
                    id: alert_id,
                    tenant_id,
                    cap_id: Some(cap.id),
                    campaign_id: cap.campaign_id,
                    period: cap.period,
                    period_start,
                    threshold: 100,
                    condition,
                    spend: total_spend.clone(),
                    cap_amount: cap.amount.clone(),
                    data_freshness: freshness.clone(),
                    auto_paused,
                    acknowledged_at: None,
                    acknowledged_by: None,
                    created_at: now,
                };

                self.alerts.record_alert(&mut scope, &alert).await?;
                self.events
                    .publish_scoped(
                        &mut scope,
                        tenant_id,
                        &[AdEvent::SpendThresholdReached {
                            campaign_id: cap.campaign_id,
                            threshold: 100,
                            alert_id: Some(alert.id),
                            scope_name: Some(scope_str.clone()),
                            spend_minor: Some(total_spend.amount_minor),
                            currency: Some(total_spend.currency.to_string()),
                            cap_minor: Some(cap.amount.amount_minor),
                            period: Some(cap.period),
                            action_taken: Some(action_taken_desc.to_string()),
                            condition: Some(condition),
                        }],
                    )
                    .await?;
                scope
                    .commit()
                    .await
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

                self.metrics
                    .budget_threshold_alerts_total()
                    .with_label_values(&[scope_metric_label, "100"])
                    .inc();
            } else {
                // Duplicate: alert already on record. Commit the read transaction but skip the
                // alert record and notification. The auto-pause was still executed above if warranted.
                scope
                    .commit()
                    .await
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
            }
        }

        Ok(())
    }

    /// Execute auto-pause for all campaigns in scope.
    ///
    /// Idempotent across sweeps: a breached cap is re-evaluated on every
    /// tick, so each in-scope campaign's stored status is re-read at
    /// mutation time and a campaign that is no longer `Active` is skipped —
    /// re-issuing the provider pause would repeat the platform API call,
    /// bump the revision, and append duplicate events and change entries on
    /// every sweep. The status read is deliberately fresher than the sweep's
    /// shared context: it must see the pauses earlier passes and manual
    /// operator actions made after that context was loaded.
    ///
    /// Returns `(paused_count, failures, already_paused_count)` where
    /// `paused_count` is the number of campaigns successfully paused this
    /// pass, `failures` holds any per-campaign errors, and
    /// `already_paused_count` counts in-scope campaigns skipped because they
    /// were not `Active`. The caller must **not** set `auto_paused = true`
    /// when `paused_count == 0`.
    async fn execute_auto_pause(
        &self,
        tenant_id: TenantId,
        cap: &BudgetCap,
        triggering_spend: &Money,
        freshness: &DataFreshness,
        condition: &str,
    ) -> (usize, Vec<AdvertisingError>, usize) {
        let metadata = serde_json::json!({
            "triggering_spend_minor": triggering_spend.amount_minor,
            "currency": triggering_spend.currency,
            "cap_amount_minor": cap.amount.amount_minor,
            "condition": condition,
            "data_freshness": freshness,
            "paused_by": "system:budget_guardrail",
        });

        let mut paused_count = 0usize;
        let mut already_paused = 0usize;
        let mut failures: Vec<AdvertisingError> = Vec::new();

        match cap.campaign_id {
            Some(campaign_id) => {
                match self.stored_campaign(tenant_id, campaign_id).await {
                    Err(e) => {
                        failures.push(e);
                        return (paused_count, failures, already_paused);
                    }
                    Ok(Some(stored)) if stored.intent.status != CampaignStatus::Active => {
                        already_paused += 1;
                    }
                    // A missing row keeps the previous behaviour: the
                    // mutation reports `CampaignNotFound` as a failure.
                    Ok(_) => {
                        let mutation_id = Uuid::new_v4();
                        match self
                            .pause_campaign
                            .execute_guardrail(
                                tenant_id,
                                campaign_id,
                                mutation_id,
                                "system:budget_guardrail",
                                metadata,
                            )
                            .await
                        {
                            Ok(_) => paused_count += 1,
                            Err(e) => {
                                tracing::error!(
                                    %tenant_id,
                                    %campaign_id,
                                    error = %e,
                                    "Budget guardrail: auto-pause failed for campaign"
                                );
                                failures.push(e);
                            }
                        }
                    }
                }
            }
            None => {
                // Pause all active campaigns for the tenant
                let campaigns = {
                    let scope_result = self.tx.begin_for(tenant_id.as_i64()).await;
                    match scope_result {
                        Err(e) => {
                            failures.push(AdvertisingError::Storage(e.to_string()));
                            return (paused_count, failures, already_paused);
                        }
                        Ok(mut scope) => {
                            let c = self.campaigns.list_by_tenant(&mut scope, tenant_id).await;
                            if let Err(e) = scope.commit().await {
                                failures.push(AdvertisingError::Storage(e.to_string()));
                                return (paused_count, failures, already_paused);
                            }
                            match c {
                                Ok(campaigns) => campaigns,
                                Err(e) => {
                                    failures.push(e);
                                    return (paused_count, failures, already_paused);
                                }
                            }
                        }
                    }
                };

                for camp in campaigns {
                    if camp.intent.status != CampaignStatus::Active {
                        already_paused += 1;
                        continue;
                    }
                    if camp.external_id.is_none() {
                        continue;
                    }
                    let mutation_id = Uuid::new_v4();
                    match self
                        .pause_campaign
                        .execute_guardrail(
                            tenant_id,
                            camp.id,
                            mutation_id,
                            "system:budget_guardrail",
                            metadata.clone(),
                        )
                        .await
                    {
                        Ok(_) => paused_count += 1,
                        Err(e) => {
                            tracing::error!(
                                %tenant_id,
                                campaign_id = %camp.id,
                                error = %e,
                                "Budget guardrail: auto-pause failed for campaign"
                            );
                            failures.push(e);
                        }
                    }
                }
            }
        }
        (paused_count, failures, already_paused)
    }

    /// Read one stored campaign in a short transaction that commits before
    /// returning.
    async fn stored_campaign(
        &self,
        tenant_id: TenantId,
        campaign_id: CampaignId,
    ) -> Result<Option<StoredCampaign>, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let stored = self
            .campaigns
            .find(&mut scope, tenant_id, campaign_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(stored)
    }
}

/// How far above a flat trailing window today's spend must land before it
/// counts as an anomaly. Sigma is undefined when every trailing day is
/// identical, so this stands in for the 3σ test there.
const FLAT_WINDOW_SPIKE_MULTIPLE: f64 = 2.0;

pub struct SpendAnomalyDetector {
    tx: TxManager,
    alerts: Arc<dyn BudgetAlertRepository>,
    metrics_repo: Arc<dyn MetricsRepository>,
    campaigns: Arc<dyn CampaignRepository>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    restatement_days: i64,
}

impl SpendAnomalyDetector {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx: TxManager,
        alerts: Arc<dyn BudgetAlertRepository>,
        metrics_repo: Arc<dyn MetricsRepository>,
        campaigns: Arc<dyn CampaignRepository>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        restatement_days: i64,
    ) -> Self {
        Self {
            tx,
            alerts,
            metrics_repo,
            campaigns,
            events,
            ids,
            restatement_days,
        }
    }

    #[tracing::instrument(name = "advertising.detect_anomalies_for_tenant", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn detect_anomalies_for_tenant(
        &self,
        tenant_id: TenantId,
    ) -> Result<(), AdvertisingError> {
        let now = Utc::now();
        let today = now.date_naive();
        let trailing_start = today - chrono::Duration::days(14);

        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let stored_campaigns = self.campaigns.list_by_tenant(&mut scope, tenant_id).await?;

        // Query daily metrics for trailing 14 days
        let query = MetricsRangeQuery {
            tenant_id,
            from: trailing_start,
            to: today,
            group_by: MetricsGroupBy::Campaign,
            platform: None,
            campaign_id: None,
        };

        let rows = self
            .metrics_repo
            .query_range(&mut scope, &query, today, self.restatement_days)
            .await?;

        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        // Check per campaign
        for camp in stored_campaigns {
            let camp_rows: Vec<_> = rows
                .iter()
                .filter(|r| r.campaign_id == Some(camp.id))
                .collect();

            let today_row = camp_rows.iter().find(|r| r.date == today);
            let historical_rows: Vec<_> = camp_rows.iter().filter(|r| r.date < today).collect();

            if let Some(today_spend_row) = today_row {
                let today_spend = today_spend_row.spend_minor;
                if historical_rows.len() >= 3 && today_spend > 0 {
                    let n = historical_rows.len() as f64;
                    let mean = historical_rows
                        .iter()
                        .map(|r| r.spend_minor as f64)
                        .sum::<f64>()
                        / n;
                    let variance = historical_rows
                        .iter()
                        .map(|r| (r.spend_minor as f64 - mean).powi(2))
                        .sum::<f64>()
                        / n;
                    let std_dev = variance.sqrt();
                    // A perfectly flat trailing window has no sigma to breach,
                    // and dividing by a zero standard deviation would call any
                    // movement at all infinitely anomalous — a campaign that
                    // spent 1000 every day for five days is not in trouble
                    // because it spent 1001 today. Fall back to a relative
                    // test there: only a genuine multiple of the established
                    // level is worth waking someone for.
                    let flat_window = std_dev <= 0.0;
                    let sigma_multiple = if flat_window {
                        f64::INFINITY
                    } else {
                        (today_spend as f64 - mean) / std_dev
                    };
                    let anomalous = if flat_window {
                        mean > 0.0 && today_spend as f64 >= mean * FLAT_WINDOW_SPIKE_MULTIPLE
                    } else {
                        sigma_multiple > 3.0
                    };

                    if anomalous {
                        let currency = &today_spend_row.currency;
                        let current_money = Money::new(today_spend, currency)?;
                        let scope_name = format!("campaign {}", camp.id);

                        tracing::warn!(
                            %tenant_id,
                            campaign_id = %camp.id,
                            today_spend,
                            mean,
                            sigma_multiple,
                            "Spend anomaly (>3sigma) detected for campaign"
                        );

                        let mut alert_scope = self
                            .tx
                            .begin_for(tenant_id.as_i64())
                            .await
                            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

                        let existing_alerts = self
                            .alerts
                            .list_alerts(&mut alert_scope, tenant_id, Some(camp.id), false, 50)
                            .await?;

                        let already_fired = existing_alerts.iter().any(|a| {
                            a.period_start == today && a.condition == AlertCondition::SpendAnomaly
                        });

                        if !already_fired {
                            let alert_id = self.ids.next_budget_alert_id()?;
                            let alert = BudgetAlert {
                                id: alert_id,
                                tenant_id,
                                cap_id: None,
                                campaign_id: Some(camp.id),
                                period: BudgetPeriod::Daily,
                                period_start: today,
                                threshold: 100,
                                condition: AlertCondition::SpendAnomaly,
                                spend: current_money.clone(),
                                cap_amount: Money::new(0, currency)?,
                                data_freshness: DataFreshness {
                                    last_synced_at: None,
                                    lag_hours: None,
                                    is_settled: false,
                                    is_stale: false,
                                },
                                auto_paused: false,
                                acknowledged_at: None,
                                acknowledged_by: None,
                                created_at: now,
                            };

                            self.alerts.record_alert(&mut alert_scope, &alert).await?;
                            self.events
                                .publish_scoped(
                                    &mut alert_scope,
                                    tenant_id,
                                    &[AdEvent::SpendThresholdReached {
                                        campaign_id: Some(camp.id),
                                        threshold: 100,
                                        alert_id: Some(alert.id),
                                        scope_name: Some(scope_name.clone()),
                                        spend_minor: Some(current_money.amount_minor),
                                        currency: Some(current_money.currency.to_string()),
                                        cap_minor: Some(0),
                                        period: None,
                                        action_taken: None,
                                        condition: Some(AlertCondition::SpendAnomaly),
                                    }],
                                )
                                .await?;
                            alert_scope
                                .commit()
                                .await
                                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
                        } else {
                            alert_scope
                                .commit()
                                .await
                                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}
