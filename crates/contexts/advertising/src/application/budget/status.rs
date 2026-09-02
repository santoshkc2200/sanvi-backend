//! Spend status calculation, run rate projection, and data freshness reporting.

use std::sync::Arc;

use chrono::{Datelike, NaiveDate, Timelike, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use utoipa::ToSchema;

use crate::domain::model::{
    BudgetActionsConfigured, BudgetPeriod, CampaignId, CampaignStatus, DataFreshness, Money,
    SpendStatusItem,
};
use crate::domain::{
    AdvertisingConnectionRepository, AdvertisingError, BudgetCapRepository, CampaignRepository,
    FxRatePort, MetricsGroupBy, MetricsRangeQuery, MetricsRepository,
};

#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct SpendStatusReport {
    pub tenant_id: String,
    pub items: Vec<SpendStatusItem>,
}

pub struct GetSpendStatus {
    tx: TxManager,
    metrics_repo: Arc<dyn MetricsRepository>,
    caps: Arc<dyn BudgetCapRepository>,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    fx_rates: Arc<dyn FxRatePort>,
    restatement_days: i64,
}

impl GetSpendStatus {
    pub fn new(
        tx: TxManager,
        metrics_repo: Arc<dyn MetricsRepository>,
        caps: Arc<dyn BudgetCapRepository>,
        campaigns: Arc<dyn CampaignRepository>,
        connections: Arc<dyn AdvertisingConnectionRepository>,
        fx_rates: Arc<dyn FxRatePort>,
        restatement_days: i64,
    ) -> Self {
        Self {
            tx,
            metrics_repo,
            caps,
            campaigns,
            connections,
            fx_rates,
            restatement_days,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        filter_period: Option<BudgetPeriod>,
    ) -> Result<SpendStatusReport, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let caps_list = self.caps.list_by_tenant(&mut scope, tenant_id).await?;
        let stored_campaigns = self.campaigns.list_by_tenant(&mut scope, tenant_id).await?;
        let connection_list = self.connections.list(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let freshness = derive_freshness(&connection_list);

        let periods_to_evaluate = match filter_period {
            Some(p) => vec![p],
            None => vec![BudgetPeriod::Daily, BudgetPeriod::Monthly],
        };

        let mut items = Vec::new();

        for period in periods_to_evaluate {
            // 1. Tenant-level item
            let tenant_cap = caps_list
                .iter()
                .find(|c| c.campaign_id.is_none() && c.period == period)
                .cloned();

            let target_currency = tenant_cap
                .as_ref()
                .map(|c| c.amount.currency.clone())
                .unwrap_or_else(|| {
                    connection_list
                        .first()
                        .map(|c| c.currency().to_string())
                        .unwrap_or_else(|| "USD".to_string())
                });

            let declared_fx = tenant_cap
                .as_ref()
                .and_then(|c| c.declared_fx_basis.as_deref());
            let fx_date = tenant_cap.as_ref().and_then(|c| c.fx_rate_date);

            // Finding 8: degrade per item — a single campaign's bad row must not abort
            // the whole report. Log and skip the item if calculation fails.
            let tenant_spend = match self
                .calculate_spend_details(
                    tenant_id,
                    None,
                    period,
                    &target_currency,
                    declared_fx,
                    fx_date,
                )
                .await
            {
                Ok(details) => details,
                Err(e) => {
                    tracing::warn!(
                        %tenant_id,
                        period = period.as_str(),
                        error = %e,
                        "Tenant-level spend calculation failed; skipping item in status report"
                    );
                    continue;
                }
            };

            let percentage = tenant_cap.as_ref().map(|c| {
                if c.amount.amount_minor > 0 {
                    (tenant_spend.total.amount_minor as f64 / c.amount.amount_minor as f64) * 100.0
                } else {
                    0.0
                }
            });

            let actions_configured = BudgetActionsConfigured {
                threshold_80: "notify".to_string(),
                threshold_100: tenant_cap
                    .as_ref()
                    .map(|c| if c.auto_pause { "pause" } else { "notify" })
                    .unwrap_or("notify")
                    .to_string(),
                auto_pause: tenant_cap.as_ref().map(|c| c.auto_pause).unwrap_or(false),
                auto_resume_on_rollover: tenant_cap
                    .as_ref()
                    .map(|c| c.auto_resume_on_rollover)
                    .unwrap_or(false),
            };

            items.push(SpendStatusItem {
                scope: "tenant".to_string(),
                campaign_id: None,
                period,
                cap: tenant_cap,
                spend_to_date: tenant_spend.total,
                settled_spend: tenant_spend.settled,
                provisional_spend: tenant_spend.provisional,
                provisional_lower_bound: Some(tenant_spend.provisional_lower_bound),
                projected_spend: tenant_spend.projected,
                percentage,
                data_freshness: freshness.clone(),
                actions_configured,
                unconvertible_spend_currencies: tenant_spend.unconvertible_currencies,
            });

            // 2. Campaign-level items
            for campaign in &stored_campaigns {
                let camp_cap = caps_list
                    .iter()
                    .find(|c| c.campaign_id == Some(campaign.id) && c.period == period)
                    .cloned();

                // If campaign is active, or has a cap, include it
                let is_active = campaign.intent.status == CampaignStatus::Active;
                if !is_active && camp_cap.is_none() {
                    continue;
                }

                let camp_currency = camp_cap
                    .as_ref()
                    .map(|c| c.amount.currency.clone())
                    .unwrap_or_else(|| campaign.intent.budget.amount.currency.clone());

                let camp_declared_fx = camp_cap
                    .as_ref()
                    .and_then(|c| c.declared_fx_basis.as_deref());
                let camp_fx_date = camp_cap.as_ref().and_then(|c| c.fx_rate_date);

                // Finding 8: degrade per campaign item
                let camp_spend = match self
                    .calculate_spend_details(
                        tenant_id,
                        Some(campaign.id),
                        period,
                        &camp_currency,
                        camp_declared_fx,
                        camp_fx_date,
                    )
                    .await
                {
                    Ok(details) => details,
                    Err(e) => {
                        tracing::warn!(
                            %tenant_id,
                            campaign_id = %campaign.id,
                            period = period.as_str(),
                            error = %e,
                            "Campaign-level spend calculation failed; skipping item in status report"
                        );
                        continue;
                    }
                };

                let camp_percentage = camp_cap.as_ref().map(|c| {
                    if c.amount.amount_minor > 0 {
                        (camp_spend.total.amount_minor as f64 / c.amount.amount_minor as f64)
                            * 100.0
                    } else {
                        0.0
                    }
                });

                let camp_actions = BudgetActionsConfigured {
                    threshold_80: "notify".to_string(),
                    threshold_100: camp_cap
                        .as_ref()
                        .map(|c| if c.auto_pause { "pause" } else { "notify" })
                        .unwrap_or("notify")
                        .to_string(),
                    auto_pause: camp_cap.as_ref().map(|c| c.auto_pause).unwrap_or(false),
                    auto_resume_on_rollover: camp_cap
                        .as_ref()
                        .map(|c| c.auto_resume_on_rollover)
                        .unwrap_or(false),
                };

                items.push(SpendStatusItem {
                    scope: format!("campaign:{}", campaign.id),
                    campaign_id: Some(campaign.id),
                    period,
                    cap: camp_cap,
                    spend_to_date: camp_spend.total,
                    settled_spend: camp_spend.settled,
                    provisional_spend: camp_spend.provisional,
                    provisional_lower_bound: Some(camp_spend.provisional_lower_bound),
                    projected_spend: camp_spend.projected,
                    percentage: camp_percentage,
                    data_freshness: freshness.clone(),
                    actions_configured: camp_actions,
                    unconvertible_spend_currencies: camp_spend.unconvertible_currencies,
                });
            }
        }

        Ok(SpendStatusReport {
            tenant_id: tenant_id.to_string(),
            items,
        })
    }

    pub async fn calculate_spend_to_date(
        &self,
        tenant_id: TenantId,
        campaign_id: Option<CampaignId>,
        period: BudgetPeriod,
        target_currency: &str,
        declared_fx_basis: Option<&str>,
        fx_rate_date: Option<NaiveDate>,
    ) -> Result<Money, AdvertisingError> {
        let details = self
            .calculate_spend_details(
                tenant_id,
                campaign_id,
                period,
                target_currency,
                declared_fx_basis,
                fx_rate_date,
            )
            .await?;
        Ok(details.total)
    }

    pub async fn calculate_spend_details(
        &self,
        tenant_id: TenantId,
        campaign_id: Option<CampaignId>,
        period: BudgetPeriod,
        target_currency: &str,
        declared_fx_basis: Option<&str>,
        fx_rate_date: Option<NaiveDate>,
    ) -> Result<CalculatedSpendDetails, AdvertisingError> {
        let now = Utc::now();
        let today = now.date_naive();

        let from_date = match period {
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

        let group_by = match campaign_id {
            Some(_) => MetricsGroupBy::Campaign,
            None => MetricsGroupBy::Tenant,
        };

        let query = MetricsRangeQuery {
            tenant_id,
            from: from_date,
            to: today,
            group_by,
            platform: None,
            campaign_id,
        };

        let rows = self
            .metrics_repo
            .query_range(&mut scope, &query, today, self.restatement_days)
            .await?;

        // A Daily period lies entirely inside the restatement window, so
        // `row.restating` is true for every row it can ever contain and cannot
        // serve as the settled/provisional gate. Connection freshness is the
        // signal that has meaning at that resolution.
        let daily_data_is_fresh = match period {
            BudgetPeriod::Daily => {
                let connections = self.connections.list(&mut scope, tenant_id).await?;
                !derive_freshness(&connections).is_stale
            }
            BudgetPeriod::Monthly => false,
        };

        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let mut settled_minor: i64 = 0;
        let mut provisional_minor: i64 = 0;
        let mut unconvertible_currencies: Vec<String> = Vec::new();

        for row in rows {
            let row_amount_minor = if row.currency == target_currency {
                row.spend_minor
            } else {
                // Cross-currency conversion
                if declared_fx_basis.is_none() {
                    // No FX basis declared: cannot convert this row. Degrade gracefully —
                    // exclude it and record the currency so the caller can surface the gap.
                    tracing::warn!(
                        %tenant_id,
                        campaign_id = ?campaign_id,
                        from_currency = %row.currency,
                        to_currency = %target_currency,
                        "FX conversion skipped: cross-currency spend has no declared FX basis; \
                         row excluded from spend totals"
                    );
                    if !unconvertible_currencies.contains(&row.currency) {
                        unconvertible_currencies.push(row.currency.clone());
                    }
                    continue;
                }

                let rate_date = fx_rate_date.unwrap_or(row.date);
                let rate_opt = self
                    .fx_rates
                    .rate(&row.currency, target_currency, rate_date)
                    .await?;

                match rate_opt {
                    Some(fx) => (row.spend_minor as f64 * fx.rate).round() as i64,
                    None => {
                        // Rate unavailable: degrade this row, log the gap, exclude from totals.
                        tracing::warn!(
                            %tenant_id,
                            campaign_id = ?campaign_id,
                            from_currency = %row.currency,
                            to_currency = %target_currency,
                            date = %rate_date,
                            "FX conversion skipped: rate unavailable for pair; \
                             row excluded from spend totals"
                        );
                        if !unconvertible_currencies.contains(&row.currency) {
                            unconvertible_currencies.push(row.currency.clone());
                        }
                        continue;
                    }
                }
            };

            // Daily: settled when the feed is fresh (see `daily_data_is_fresh`).
            // Monthly: settled once the row is outside the restatement window.
            let is_settled = match period {
                BudgetPeriod::Daily => daily_data_is_fresh,
                BudgetPeriod::Monthly => !row.restating,
            };
            if is_settled {
                settled_minor += row_amount_minor;
            } else {
                provisional_minor += row_amount_minor;
            }
        }

        let total_minor = settled_minor + provisional_minor;
        let lower_bound_minor = settled_minor + (provisional_minor as f64 * 0.8).round() as i64;

        // Run rate projection
        let elapsed_fraction = calculate_elapsed_fraction(now, period);
        let projected_minor = if elapsed_fraction > 0.05 {
            (total_minor as f64 / elapsed_fraction).round() as i64
        } else {
            total_minor
        };

        Ok(CalculatedSpendDetails {
            settled: Money::new(settled_minor, target_currency)?,
            provisional: Money::new(provisional_minor, target_currency)?,
            provisional_lower_bound: Money::new(lower_bound_minor, target_currency)?,
            total: Money::new(total_minor, target_currency)?,
            projected: Money::new(projected_minor, target_currency)?,
            unconvertible_currencies,
        })
    }
}

pub struct CalculatedSpendDetails {
    pub settled: Money,
    pub provisional: Money,
    pub provisional_lower_bound: Money,
    pub total: Money,
    pub projected: Money,
    /// Currencies for which no FX rate was available; spend in these currencies
    /// is excluded from the totals above.
    pub unconvertible_currencies: Vec<String>,
}

/// Derive the data-freshness summary from a slice of advertising connections.
///
/// Matching the evaluator's staleness check:
/// - A connection whose `last_synced_at` is `None` (never synced) is treated
///   as stale, so `GET /ads/spend-status` and the evaluator agree.
/// - With zero connections: data is neither settled nor stale (`lag_hours: None`);
///   both `is_settled` and `is_stale` are false, which is the correct "no data"
///   state rather than the misleading "fresh but empty" state.
fn derive_freshness(connections: &[crate::domain::AdvertisingConnection]) -> DataFreshness {
    if connections.is_empty() {
        return DataFreshness {
            last_synced_at: None,
            lag_hours: None,
            is_settled: false,
            is_stale: false,
        };
    }

    let mut latest_sync = None;
    let mut worst_lag: Option<f64> = None;
    let mut any_stalled = false;

    let now = Utc::now();
    for conn in connections {
        match conn.last_synced_at() {
            None => {
                // Never synced → unconditionally stale, matching evaluator.rs
                any_stalled = true;
            }
            Some(synced) => {
                if latest_sync.is_none() || Some(synced) > latest_sync {
                    latest_sync = Some(synced);
                }
                let lag = (now - synced).num_minutes() as f64 / 60.0;
                if worst_lag.is_none() || Some(lag) > worst_lag {
                    worst_lag = Some(lag);
                }
                if lag > 6.0 {
                    any_stalled = true;
                }
            }
        }
    }

    DataFreshness {
        last_synced_at: latest_sync,
        lag_hours: worst_lag,
        is_settled: worst_lag.map(|l| l < 6.0).unwrap_or(false) && !any_stalled,
        is_stale: any_stalled,
    }
}

fn calculate_elapsed_fraction(now: chrono::DateTime<Utc>, period: BudgetPeriod) -> f64 {
    match period {
        BudgetPeriod::Daily => {
            let seconds_today = now.num_seconds_from_midnight() as f64;
            (seconds_today / 86400.0).clamp(0.01, 1.0)
        }
        BudgetPeriod::Monthly => {
            let day = now.day() as f64;
            let seconds_today = now.num_seconds_from_midnight() as f64;
            let days_in_month = match now.month() {
                1 | 3 | 5 | 7 | 8 | 10 | 12 => 31.0,
                4 | 6 | 9 | 11 => 30.0,
                2 => {
                    let year = now.year();
                    if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                        29.0
                    } else {
                        28.0
                    }
                }
                _ => 30.0,
            };
            let elapsed_days = (day - 1.0) + (seconds_today / 86400.0);
            (elapsed_days / days_in_month).clamp(0.01, 1.0)
        }
    }
}
