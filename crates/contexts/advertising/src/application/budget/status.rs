//! Spend status calculation, run rate projection, and data freshness reporting.

use std::collections::HashMap;
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
    AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError, BudgetCapRepository,
    CampaignRepository, FxRatePort, MetricsGroupBy, MetricsRangeQuery, MetricsRepository,
    StoredCampaign, TenantTimezoneSource,
};

#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct SpendStatusReport {
    pub tenant_id: String,
    /// The tenant calendar timezone the report's period windows were
    /// computed in (resolved from the tenant's locale settings). Surfaced on
    /// the report so clients holding advertising access — but not
    /// locale-settings access — render projections on the same clock
    /// enforcement uses.
    pub timezone: String,
    pub items: Vec<SpendStatusItem>,
}

/// Everything the spend arithmetic needs about a tenant, loaded once per
/// report or guardrail sweep instead of once per item or cap. Shared with
/// the threshold evaluator so a sweep's cap evaluations reuse one read of
/// the tenant's connections, campaigns, and timezone.
pub(crate) struct SpendContext {
    pub(crate) connections: Vec<AdvertisingConnection>,
    pub(crate) campaigns: Vec<StoredCampaign>,
    /// The tenant calendar timezone by name — the same clock the report's
    /// period windows use, surfaced on the report so clients holding
    /// advertising access render projections on the enforcement clock.
    pub(crate) tz_name: String,
    pub(crate) now: chrono::DateTime<chrono_tz::Tz>,
}

pub struct GetSpendStatus {
    tx: TxManager,
    metrics_repo: Arc<dyn MetricsRepository>,
    caps: Arc<dyn BudgetCapRepository>,
    campaigns: Arc<dyn CampaignRepository>,
    connections: Arc<dyn AdvertisingConnectionRepository>,
    fx_rates: Arc<dyn FxRatePort>,
    timezones: Arc<dyn TenantTimezoneSource>,
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
        timezones: Arc<dyn TenantTimezoneSource>,
        restatement_days: i64,
    ) -> Self {
        Self {
            tx,
            metrics_repo,
            caps,
            campaigns,
            connections,
            fx_rates,
            timezones,
            restatement_days,
        }
    }

    pub async fn execute(
        &self,
        tenant_id: TenantId,
        filter_period: Option<BudgetPeriod>,
    ) -> Result<SpendStatusReport, AdvertisingError> {
        // One context for the whole report: connections, campaigns, and the
        // tenant timezone are shared by the tenant item and every campaign
        // item across both periods — reloading them per item made a report
        // 2(C+1) full reads with quadratic row transfer.
        let context = self.load_spend_context(tenant_id).await?;

        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let caps_list = self.caps.list_by_tenant(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let tenant_freshness = derive_freshness(&context.connections);

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
                    context
                        .connections
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
                .calculate_from_context(
                    &context,
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
                data_freshness: tenant_freshness.clone(),
                actions_configured,
                unconvertible_spend_currencies: tenant_spend.unconvertible_currencies,
            });

            // 2. Campaign-level items
            for campaign in &context.campaigns {
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
                    .calculate_from_context(
                        &context,
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

                // Campaign items are fresh exactly when the connection feeding
                // that campaign has ingested recently — never the tenant-wide
                // aggregate, which an unrelated stale connection would poison.
                let camp_freshness = freshness_for_scope(
                    &context.connections,
                    &context.campaigns,
                    Some(campaign.id),
                );

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
                    data_freshness: camp_freshness,
                    actions_configured: camp_actions,
                    unconvertible_spend_currencies: camp_spend.unconvertible_currencies,
                });
            }
        }

        Ok(SpendStatusReport {
            tenant_id: tenant_id.to_string(),
            timezone: context.tz_name.clone(),
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

    /// Pair-independent snapshot existence for the pinned-basis validation:
    /// a cap's `fx_rate_date` must resolve to its own day before the cap is
    /// saved, regardless of what — if anything — currently spends in scope.
    pub async fn has_fx_snapshot(&self, date: NaiveDate) -> Result<bool, AdvertisingError> {
        self.fx_rates.has_snapshot(date).await
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
        let context = self.load_spend_context(tenant_id).await?;
        self.calculate_from_context(
            &context,
            tenant_id,
            campaign_id,
            period,
            target_currency,
            declared_fx_basis,
            fx_rate_date,
        )
        .await
    }

    /// Everything the spend arithmetic needs about a tenant, loaded once per
    /// report or sweep instead of once per item or cap: `execute` evaluates
    /// the tenant scope and every campaign across up to two periods, and the
    /// guardrail evaluator evaluates every cap of a sweep against it —
    /// reloading the connection list, the campaign list, and the timezone
    /// per item or cap made a report 2(C+1) full reads and a sweep's spend
    /// lookups transfer O(C²) campaign rows.
    pub(crate) async fn load_spend_context(
        &self,
        tenant_id: TenantId,
    ) -> Result<SpendContext, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let connections = self.connections.list(&mut scope, tenant_id).await?;
        let campaigns = self.campaigns.list_by_tenant(&mut scope, tenant_id).await?;
        let tz_name = self.timezones.timezone(&mut scope, tenant_id).await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let tz = parse_tenant_timezone(&tz_name);
        let now = Utc::now().with_timezone(&tz);
        Ok(SpendContext {
            connections,
            campaigns,
            tz_name,
            now,
        })
    }

    /// The spend arithmetic for one scope+period, from a preloaded
    /// [`SpendContext`]. Opens its own short transaction for the metrics
    /// read and commits it before conversion begins. Callers evaluating
    /// many scopes of one tenant (the status report, a guardrail sweep)
    /// load the context once and pass it to every call.
    pub(crate) async fn calculate_from_context(
        &self,
        context: &SpendContext,
        tenant_id: TenantId,
        campaign_id: Option<CampaignId>,
        period: BudgetPeriod,
        target_currency: &str,
        declared_fx_basis: Option<&str>,
        fx_rate_date: Option<NaiveDate>,
    ) -> Result<CalculatedSpendDetails, AdvertisingError> {
        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        // Period boundaries are computed in the tenant's calendar timezone
        // (TASK-017), resolved from the tenant's locale settings — never UTC
        // and never an ad account's timezone. The metrics rows themselves are
        // daily grain, so the window is [from_date, today] in that timezone.
        let now = context.now;
        let today = now.date_naive();

        let from_date = match period {
            BudgetPeriod::Daily => today,
            BudgetPeriod::Monthly => {
                NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today)
            }
        };

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
        // serve as the settled/provisional gate. Metrics-ingestion freshness
        // of the connections feeding this scope is the signal that has
        // meaning at that resolution.
        let daily_data_is_fresh = match period {
            BudgetPeriod::Daily => {
                let scoped = match campaign_id {
                    Some(_) => {
                        freshness_for_scope(&context.connections, &context.campaigns, campaign_id)
                    }
                    None => derive_freshness(&context.connections),
                };
                !scoped.is_stale
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
        // A pinned date resolves the same (pair, day) for every row that
        // shares its currency — a month of rows would otherwise repeat an
        // identical durable-store lookup (its own transaction each) per row.
        // Resolutions are memoized per `(from, to, date)`: the lookups, the
        // exactness checks, and the degrade logs all happen once, and a
        // memoized `None` simply excludes the row again.
        let mut resolved: HashMap<(String, String, NaiveDate), Option<f64>> = HashMap::new();

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

                // A pinned basis (`fx_rate_date` on the cap — the date the
                // audit trail persists) must resolve to *exactly* that day's
                // snapshot: `FxRate::date` is what the provider actually
                // used, and a previous-close substitution — or a feed whose
                // window has moved on — would make the persisted basis and
                // the computed threshold silently disagree, with the same
                // request totalling differently as the feed changes. Undated
                // rows fall back to their own day under the feed's
                // previous-close convention, where answering Friday for a
                // weekend is the designed behaviour; there the resolved date
                // is what the degrade log carries.
                let rate_date = fx_rate_date.unwrap_or(row.date);
                let key = (row.currency.clone(), target_currency.to_string(), rate_date);
                let validated_rate = match resolved.get(&key) {
                    Some(cached) => *cached,
                    None => {
                        let rate_opt = self
                            .fx_rates
                            .rate(&row.currency, target_currency, rate_date)
                            .await?;
                        let validated = match rate_opt {
                            Some(fx) if fx_rate_date.is_none() || fx.date == rate_date => {
                                Some(fx.rate)
                            }
                            Some(fx) => {
                                // The provider substituted a neighbouring
                                // snapshot for a pinned date: refuse the
                                // conversion rather than compute a threshold
                                // the persisted basis cannot reproduce.
                                tracing::warn!(
                                    %tenant_id,
                                    campaign_id = ?campaign_id,
                                    from_currency = %row.currency,
                                    to_currency = %target_currency,
                                    requested = %rate_date,
                                    resolved = %fx.date,
                                    "FX snapshot does not match the pinned basis date; \
                                     row excluded from spend totals"
                                );
                                None
                            }
                            None => {
                                // Rate unavailable: degrade this row, log the
                                // gap, exclude from totals.
                                tracing::warn!(
                                    %tenant_id,
                                    campaign_id = ?campaign_id,
                                    from_currency = %row.currency,
                                    to_currency = %target_currency,
                                    date = %rate_date,
                                    "FX conversion skipped: rate unavailable for pair; \
                                     row excluded from spend totals"
                                );
                                None
                            }
                        };
                        resolved.insert(key.clone(), validated);
                        validated
                    }
                };
                match validated_rate {
                    Some(rate) => {
                        convert_minor_units(row.spend_minor, &row.currency, target_currency, rate)
                    }
                    None => {
                        // The degrade was logged (once) at resolution time;
                        // every row sharing the pair is excluded silently.
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

/// Parse the tenant's IANA timezone name for period boundaries.
///
/// TASK-017 defines budget periods in the *tenant* timezone (from the
/// tenant's locale settings): "today" for a Tokyo tenant is a different
/// spend window than UTC's, and an ad account's timezone is the platform's
/// reporting detail, not the tenant's calendar. Both tenant and campaign
/// scopes share the one tenant clock, so mixed-account tenants get a single
/// coherent window — and the frontend preview, which reads the same tenant
/// setting, cannot disagree with enforcement.
///
/// An unparseable name fails open to UTC with a warning rather than failing
/// the whole report.
pub(crate) fn parse_tenant_timezone(raw: &str) -> chrono_tz::Tz {
    raw.parse().unwrap_or_else(|_| {
        tracing::warn!(
            timezone = %raw,
            "Budget period: unparseable tenant timezone, falling back to UTC"
        );
        chrono_tz::UTC
    })
}

/// Derive the data-freshness summary from advertising connections.
///
/// The cursor is the metrics-ingestion cursor
/// (`last_metrics_ingested_at`), **not** the OAuth `last_synced_at`: a token
/// refresh must never make stale spend look safe for auto-pause, and an
/// unrelated connection's OAuth activity must not affect a cap it cannot
/// feed. This matches the evaluator's staleness check and the
/// `/ads/metrics/freshness` endpoint, which both read the same cursor.
///
/// - A connection whose `last_metrics_ingested_at` is `None` (never ingested)
///   is treated as stale.
/// - With zero connections: data is neither settled nor stale
///   (`lag_hours: None`); both `is_settled` and `is_stale` are false, which is
///   the correct "no data" state rather than the misleading "fresh but empty"
///   state.
///
/// `last_synced_at` on the report keeps its wire name for API compatibility
/// but carries the latest ingestion time — the moment the spend figures were
/// actually current — for the same reason.
pub(crate) fn derive_freshness(connections: &[AdvertisingConnection]) -> DataFreshness {
    freshness_for_scope(connections, &[], None)
}

/// Freshness for one evaluation scope.
///
/// - `Some(campaign_id)`: only the connections feeding that campaign count.
///   A stale connection on an unrelated ad account must not suppress a valid
///   campaign pause, and a fresh unrelated connection must not make a stale
///   campaign's spend look safe. A campaign missing from the context, or
///   whose connection is gone, vouches nothing: the result is stale (fail
///   closed), never the settled branch.
/// - `None` (tenant scope): every connection counts, since tenant spend sums
///   all of them; zero connections is the "no data" state, not staleness.
pub(crate) fn freshness_for_scope(
    connections: &[AdvertisingConnection],
    campaigns: &[StoredCampaign],
    campaign_id: Option<CampaignId>,
) -> DataFreshness {
    let relevant: Vec<&AdvertisingConnection> = match campaign_id {
        Some(wanted) => {
            let wanted_connection = campaigns
                .iter()
                .find(|c| c.id == wanted)
                .map(|c| c.connection_id);
            match wanted_connection {
                Some(connection_id) => connections
                    .iter()
                    .filter(|c| c.id() == connection_id)
                    .collect(),
                // Unknown campaign: no connection can vouch for its data.
                None => Vec::new(),
            }
        }
        None => connections.iter().collect(),
    };

    if relevant.is_empty() {
        // Tenant scope with zero connections has structurally zero spend —
        // no rows to sum, nothing vouching needed — which is the "no data"
        // state, not staleness. Campaign scope with an empty relevant set is
        // the opposite: the campaign is unknown to the context or its
        // account disconnected while its recorded spend remains, and no
        // connection vouches for it. That must read as stale, or the
        // evaluator's settled branch auto-pauses on unverified data.
        let unverified = campaign_id.is_some();
        return DataFreshness {
            last_synced_at: None,
            lag_hours: None,
            is_settled: false,
            is_stale: unverified,
        };
    }

    let mut latest_ingest = None;
    let mut worst_lag: Option<f64> = None;
    let mut any_stalled = false;

    let now = Utc::now();
    for conn in relevant {
        match conn.last_metrics_ingested_at() {
            None => {
                // Never ingested → unconditionally stale, matching evaluator.rs
                any_stalled = true;
            }
            Some(ingested) => {
                if latest_ingest.is_none() || Some(ingested) > latest_ingest {
                    latest_ingest = Some(ingested);
                }
                let lag = (now - ingested).num_minutes() as f64 / 60.0;
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
        last_synced_at: latest_ingest,
        lag_hours: worst_lag,
        is_settled: worst_lag.map(|l| l < 6.0).unwrap_or(false) && !any_stalled,
        is_stale: any_stalled,
    }
}

/// Convert `amount_minor` minor units of `from` into minor units of `to` at
/// `rate`, where `rate` converts one *major* unit of `from` into `to` (the
/// [`FxRatePort`](crate::domain::FxRatePort) contract).
///
/// The path is minor → major → major → minor: `$1` (100 USD minor) at
/// USD→JPY 156.18 is ¥156 (156 JPY minor), *not* 100 × 156.18 = ¥15,618.
/// Multiplying minor units by the bare rate is 100× too large across a 2↔0
/// exponent boundary (and 100× too small the other way) — enough to
/// auto-pause on a phantom breach.
pub(crate) fn convert_minor_units(amount_minor: i64, from: &str, to: &str, rate: f64) -> i64 {
    let scale = 10f64.powi(Money::exponent(to) as i32 - Money::exponent(from) as i32);
    (amount_minor as f64 * rate * scale).round() as i64
}

/// Fraction of the period elapsed at `now`, measured in the scope's timezone:
/// "seconds since midnight" must be midnight on the ad account's clock, or a
/// tenant east of UTC projects a full day's run rate from a few minutes of
/// data (and one west of UTC undershoots the same way).
fn calculate_elapsed_fraction<Tz: chrono::TimeZone>(
    now: chrono::DateTime<Tz>,
    period: BudgetPeriod,
) -> f64 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Budget, BudgetType, Campaign, DriftState, Objective};
    use crate::domain::{ConnectionId, PlatformKey, SecretRef, StoredCampaign};
    use chrono::Duration;

    fn tenant(n: i64) -> TenantId {
        TenantId::try_from(n).unwrap()
    }

    fn connection(n: i64, timezone: &str) -> AdvertisingConnection {
        AdvertisingConnection::new(
            ConnectionId::try_from(n).unwrap(),
            tenant(9_000_001),
            PlatformKey::parse("network_a").unwrap(),
            format!("account-{n}"),
            "JPY",
            timezone,
            SecretRef::new("opaque-vault-reference"),
            None,
            Utc::now(),
        )
        .unwrap()
    }

    fn campaign(id: i64, connection_id: ConnectionId) -> StoredCampaign {
        let intent = Campaign {
            id: "external-1".to_string(),
            name: "Test".to_string(),
            status: CampaignStatus::Active,
            objective: Objective::parse("sales").unwrap(),
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(10_000, "JPY").unwrap(),
            },
            schedule: None,
            ad_groups: Vec::new(),
            drift: DriftState {
                drifted: false,
                changed_fields: Vec::new(),
            },
        };
        StoredCampaign {
            id: CampaignId::try_from(id).unwrap(),
            tenant_id: tenant(9_000_001),
            connection_id,
            platform: PlatformKey::parse("network_a").unwrap(),
            external_id: Some("external-1".to_string()),
            revision: 1,
            intent: intent.clone(),
            remote_snapshot: intent,
            publishing_mutation_id: None,
        }
    }

    #[test]
    fn tenant_timezone_parses_iana_names_and_falls_back_to_utc() {
        assert_eq!(parse_tenant_timezone("Asia/Tokyo"), chrono_tz::Asia::Tokyo);
        assert_eq!(
            parse_tenant_timezone("America/New_York"),
            chrono_tz::America::New_York
        );
        assert_eq!(parse_tenant_timezone("Not/AZone"), chrono_tz::UTC);
        assert_eq!(parse_tenant_timezone(""), chrono_tz::UTC);
    }

    #[test]
    fn fx_conversion_applies_currency_exponents() {
        // $1 (100 USD minor) at USD→JPY 156.18 is ¥156 — the bare
        // multiplication (15,618) would auto-pause on a phantom breach.
        assert_eq!(convert_minor_units(100, "USD", "JPY", 156.18), 156);
        // And the reverse: ¥169 at JPY→USD ≈ $1.08 (108 USD minor), not 1.
        assert_eq!(convert_minor_units(169, "JPY", "USD", 1.0 / 156.18), 108);
        // Same-exponent pairs are the plain rate.
        assert_eq!(convert_minor_units(10_000, "USD", "EUR", 0.92), 9_200);
        // Three-decimal targets scale up: $1 (100 USD minor) at USD→BHD
        // 0.376 is 0.376 BHD = 376 fils.
        assert_eq!(convert_minor_units(100, "USD", "BHD", 0.376), 376);
        // Unknown codes fall back to exponent 2 rather than failing.
        assert_eq!(Money::exponent("jpy"), 0);
        assert_eq!(Money::exponent("KWD"), 3);
        assert_eq!(Money::exponent("ZZZ"), 2);
    }

    #[test]
    fn freshness_follows_ingestion_not_oauth_sync() {
        let now = Utc::now();
        // A token refresh just ran, but metrics ingestion never did: stale.
        let mut refreshed_only = connection(1_000_001, "Asia/Tokyo");
        refreshed_only.record_refresh_success(None, now);
        let fresh = derive_freshness(&[refreshed_only]);
        assert!(fresh.is_stale);
        assert!(!fresh.is_settled);

        // Ingestion ran recently, OAuth untouched: settled.
        let mut ingested = connection(1_000_002, "Asia/Tokyo");
        ingested.record_metrics_sync(now);
        let fresh = derive_freshness(&[ingested]);
        assert!(!fresh.is_stale);
        assert!(fresh.is_settled);

        // Stale ingestion (8h ago) is stale even with a fresh token.
        let mut old = connection(1_000_003, "Asia/Tokyo");
        old.record_refresh_success(None, now);
        old.record_metrics_sync(now - Duration::hours(8));
        assert!(derive_freshness(&[old]).is_stale);
    }

    #[test]
    fn campaign_freshness_ignores_unrelated_connections() {
        let now = Utc::now();
        let mut fresh_conn = connection(1_000_001, "Asia/Tokyo");
        fresh_conn.record_metrics_sync(now);
        let stale_conn = connection(1_000_002, "Asia/Tokyo");
        let connections = vec![fresh_conn, stale_conn];
        let campaigns = vec![campaign(
            2_000_001,
            ConnectionId::try_from(1_000_001).unwrap(),
        )];

        // The campaign's own feed is fresh: not stale despite the unrelated
        // stale connection.
        let scoped = freshness_for_scope(
            &connections,
            &campaigns,
            Some(CampaignId::try_from(2_000_001).unwrap()),
        );
        assert!(!scoped.is_stale);

        // Tenant scope still sees the stale connection.
        assert!(derive_freshness(&connections).is_stale);
    }

    #[test]
    fn campaign_scope_with_no_vouching_connection_fails_closed() {
        let now = Utc::now();
        let mut fresh_conn = connection(1_000_001, "Asia/Tokyo");
        fresh_conn.record_metrics_sync(now);
        let campaigns = vec![campaign(
            2_000_001,
            ConnectionId::try_from(1_000_001).unwrap(),
        )];

        // The campaign's account disconnected: its recorded spend remains but
        // nothing vouches for it, so the scope reads stale — never the
        // settled branch that auto-pauses.
        let disconnected = freshness_for_scope(
            &[],
            &campaigns,
            Some(CampaignId::try_from(2_000_001).unwrap()),
        );
        assert!(disconnected.is_stale);
        assert!(!disconnected.is_settled);

        // A cap scoped to a campaign the context does not hold fails closed
        // the same way.
        let unknown = freshness_for_scope(
            &[fresh_conn],
            &campaigns,
            Some(CampaignId::try_from(2_000_002).unwrap()),
        );
        assert!(unknown.is_stale);

        // Tenant scope with zero connections stays "no data" (spend is
        // structurally zero), not stale.
        let no_data = derive_freshness(&[]);
        assert!(!no_data.is_stale);
        assert!(!no_data.is_settled);
    }
}
