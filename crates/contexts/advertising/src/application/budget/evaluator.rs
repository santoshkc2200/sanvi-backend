//! Budget threshold evaluation, provisional data guard, auto-pause execution,
//! and spend anomaly detection (TASK-017, slice 10.8).

use std::sync::Arc;

use chrono::{Datelike, NaiveDate, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::{PlatformScope, TxManager};
use uuid::Uuid;

use crate::adapters::AdvertisingMetrics;
use crate::domain::model::{
    AlertCondition, BudgetAlert, BudgetCap, BudgetPeriod, CampaignStatus, DataFreshness, Money,
};
use crate::domain::{
    AdEvent, AdvertisingConnectionRepository, AdvertisingError, BudgetAlertRepository,
    BudgetCapRepository, BudgetNotifier, CampaignRepository, EventPublisher, IdGenerator,
    MetricsGroupBy, MetricsRangeQuery, MetricsRepository,
};

pub struct ThresholdEvaluator {
    tx: TxManager,
    caps: Arc<dyn BudgetCapRepository>,
    alerts: Arc<dyn BudgetAlertRepository>,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    pause_campaign: Arc<crate::application::campaigns::PauseCampaign>,
    status_query: Arc<super::status::GetSpendStatus>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    notifier: Arc<dyn BudgetNotifier>,
    metrics: Arc<AdvertisingMetrics>,
}

impl ThresholdEvaluator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx: TxManager,
        caps: Arc<dyn BudgetCapRepository>,
        alerts: Arc<dyn BudgetAlertRepository>,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        pause_campaign: Arc<crate::application::campaigns::PauseCampaign>,
        status_query: Arc<super::status::GetSpendStatus>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        notifier: Arc<dyn BudgetNotifier>,
        metrics: Arc<AdvertisingMetrics>,
    ) -> Self {
        Self {
            tx,
            caps,
            alerts,
            campaigns,
            connections,
            pause_campaign,
            status_query,
            events,
            ids,
            notifier,
            metrics,
        }
    }

    pub async fn evaluate_all(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<(), AdvertisingError> {
        let caps = self.caps.list_all_active_caps(scope).await?;
        for cap in caps {
            let _ = self.evaluate_single_cap(cap.tenant_id, &cap).await;
        }
        Ok(())
    }

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

        for cap in caps {
            let _ = self.evaluate_single_cap(tenant_id, &cap).await;
        }
        Ok(())
    }

    pub async fn evaluate_single_cap(
        &self,
        tenant_id: TenantId,
        cap: &BudgetCap,
    ) -> Result<(), AdvertisingError> {
        let now = Utc::now();
        let today = now.date_naive();
        let period_start = match cap.period {
            BudgetPeriod::Daily => today,
            BudgetPeriod::Monthly => {
                NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today)
            }
        };

        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let connection_list = self.connections.list(&mut scope, tenant_id).await?;
        let is_stale = connection_list.iter().any(|c| {
            if let Some(synced) = c.last_synced_at() {
                (now - synced).num_hours() > 6
            } else {
                true
            }
        });

        let freshness = DataFreshness {
            last_synced_at: connection_list
                .iter()
                .filter_map(|c| c.last_synced_at())
                .max(),
            lag_hours: connection_list
                .iter()
                .filter_map(|c| {
                    c.last_synced_at()
                        .map(|s| (now - s).num_minutes() as f64 / 60.0)
                })
                .reduce(f64::max),
            is_settled: !is_stale,
            is_stale,
        };

        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        // Calculate spend details
        let spend_details = match self
            .status_query
            .calculate_spend_details(
                tenant_id,
                cap.campaign_id,
                cap.period,
                &cap.amount.currency,
                cap.declared_fx_basis.as_deref(),
                cap.fx_rate_date,
            )
            .await
        {
            Ok(details) => details,
            Err(AdvertisingError::InvalidInput(msg))
                if msg.contains("requires declared FX basis") =>
            {
                tracing::warn!(%tenant_id, cap_id = %cap.id, "Refusing budget evaluation: cross-currency cap requires declared FX basis");
                return Err(AdvertisingError::InvalidInput(msg));
            }
            Err(e) => return Err(e),
        };

        let total_spend = spend_details.total;
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

                let _ = self
                    .notifier
                    .notify_threshold_80(
                        tenant_id,
                        &scope_str,
                        &total_spend,
                        &cap.amount,
                        cap.period,
                    )
                    .await;
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

            if !already_fired {
                let mut auto_paused = false;
                let (condition, action_taken_desc) = if is_stale {
                    (
                        AlertCondition::StaleDataWarning,
                        "Guardrails will not pause on unconfirmed data.",
                    )
                } else if spend_details.settled.amount_minor >= cap_minor {
                    if cap.auto_pause {
                        self.execute_auto_pause(
                            tenant_id,
                            cap,
                            &total_spend,
                            &freshness,
                            "settled_breach",
                        )
                        .await?;
                        auto_paused = true;
                        self.metrics
                            .budget_auto_pause_total()
                            .with_label_values(&["settled_breach"])
                            .inc();
                        (
                            AlertCondition::SettledBreach,
                            "Campaign(s) auto-paused by budget guardrail.",
                        )
                    } else {
                        (
                            AlertCondition::SettledBreach,
                            "No automatic pause configured.",
                        )
                    }
                } else if spend_details.provisional_lower_bound.amount_minor >= cap_minor {
                    if cap.auto_pause {
                        self.execute_auto_pause(
                            tenant_id,
                            cap,
                            &total_spend,
                            &freshness,
                            "provisional_lower_bound_breach",
                        )
                        .await?;
                        auto_paused = true;
                        self.metrics
                            .budget_auto_pause_total()
                            .with_label_values(&["provisional_lower_bound_breach"])
                            .inc();
                        (
                            AlertCondition::ProvisionalLowerBoundBreach,
                            "Campaign(s) auto-paused on confirmed provisional lower bound breach.",
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

                // Dispatch notification based on condition
                match condition {
                    AlertCondition::StaleDataWarning => {
                        let _ = self
                            .notifier
                            .notify_stale_warning(
                                tenant_id,
                                &scope_str,
                                freshness.lag_hours.unwrap_or(0.0),
                            )
                            .await;
                    }
                    AlertCondition::ProvisionalWarning => {
                        let _ = self
                            .notifier
                            .notify_provisional_warning(
                                tenant_id,
                                &scope_str,
                                &total_spend,
                                &cap.amount,
                                &spend_details.provisional_lower_bound,
                                cap.period,
                            )
                            .await;
                    }
                    AlertCondition::SettledBreach | AlertCondition::ProvisionalLowerBoundBreach => {
                        let _ = self
                            .notifier
                            .notify_cap_breached(
                                tenant_id,
                                &scope_str,
                                &total_spend,
                                &cap.amount,
                                cap.period,
                                action_taken_desc,
                            )
                            .await;
                    }
                    _ => {}
                }
            } else {
                scope
                    .commit()
                    .await
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
            }
        }

        Ok(())
    }

    async fn execute_auto_pause(
        &self,
        tenant_id: TenantId,
        cap: &BudgetCap,
        triggering_spend: &Money,
        freshness: &DataFreshness,
        condition: &str,
    ) -> Result<(), AdvertisingError> {
        let metadata = serde_json::json!({
            "triggering_spend_minor": triggering_spend.amount_minor,
            "currency": triggering_spend.currency,
            "cap_amount_minor": cap.amount.amount_minor,
            "condition": condition,
            "data_freshness": freshness,
            "paused_by": "system:budget_guardrail",
        });

        match cap.campaign_id {
            Some(campaign_id) => {
                let mutation_id = Uuid::new_v4();
                let _ = self
                    .pause_campaign
                    .execute_guardrail(
                        tenant_id,
                        campaign_id,
                        mutation_id,
                        "system:budget_guardrail",
                        metadata,
                    )
                    .await;
            }
            None => {
                // Pause all active campaigns for the tenant
                let mut scope = self
                    .tx
                    .begin_for(tenant_id.as_i64())
                    .await
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
                let campaigns = self.campaigns.list_by_tenant(&mut scope, tenant_id).await?;
                scope
                    .commit()
                    .await
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

                for camp in campaigns {
                    if camp.intent.status == CampaignStatus::Active && camp.external_id.is_some() {
                        let mutation_id = Uuid::new_v4();
                        let _ = self
                            .pause_campaign
                            .execute_guardrail(
                                tenant_id,
                                camp.id,
                                mutation_id,
                                "system:budget_guardrail",
                                metadata.clone(),
                            )
                            .await;
                    }
                }
            }
        }
        Ok(())
    }
}

/// How far above a flat trailing window today's spend must land before it
/// counts as an anomaly. Sigma is undefined when every trailing day is
/// identical, so this stands in for the 3σ test there.
const FLAT_WINDOW_SPIKE_MULTIPLE: f64 = 2.0;

/// Bound on the cross-tenant connection scan the anomaly sweep enumerates
/// tenants from, matching the platform-health read.
const TENANT_ENUMERATION_LIMIT: i64 = 5_000;

pub struct SpendAnomalyDetector {
    tx: TxManager,
    alerts: Arc<dyn BudgetAlertRepository>,
    metrics_repo: Arc<dyn MetricsRepository>,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    events: Arc<dyn EventPublisher>,
    ids: Arc<dyn IdGenerator>,
    notifier: Arc<dyn BudgetNotifier>,
    restatement_days: i64,
}

impl SpendAnomalyDetector {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx: TxManager,
        alerts: Arc<dyn BudgetAlertRepository>,
        metrics_repo: Arc<dyn MetricsRepository>,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn IdGenerator>,
        notifier: Arc<dyn BudgetNotifier>,
        restatement_days: i64,
    ) -> Self {
        Self {
            tx,
            alerts,
            metrics_repo,
            campaigns,
            connections,
            events,
            ids,
            notifier,
            restatement_days,
        }
    }

    /// Every tenant with an advertising connection, not just those whose
    /// campaigns happen to be due for *reconciliation* — the two schedules are
    /// unrelated, and enumerating from the reconciliation due-list would make
    /// anomaly detection silently skip whichever tenants that list omits.
    pub async fn detect_anomalies_all(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<(), AdvertisingError> {
        let connections = self
            .connections
            .list_all_for_platform_scope(scope, TENANT_ENUMERATION_LIMIT)
            .await?;
        let tenants: std::collections::BTreeSet<TenantId> =
            connections.iter().map(|conn| conn.tenant_id()).collect();
        for tenant_id in tenants {
            // One tenant's failure must not end the sweep for the rest.
            if let Err(error) = self.detect_anomalies_for_tenant(tenant_id).await {
                tracing::warn!(
                    %tenant_id,
                    %error,
                    "spend anomaly detection failed for tenant"
                );
            }
        }
        Ok(())
    }

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
                        let mean_money = Money::new(mean.round() as i64, currency)?;
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
                                    }],
                                )
                                .await?;
                            alert_scope
                                .commit()
                                .await
                                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

                            let _ = self
                                .notifier
                                .notify_spend_anomaly(
                                    tenant_id,
                                    &scope_name,
                                    &current_money,
                                    &mean_money,
                                    sigma_multiple,
                                )
                                .await;
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
