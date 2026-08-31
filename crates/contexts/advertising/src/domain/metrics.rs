//! Persisted daily metrics rollup and the ROAS query layer (TASK-016, slice 10.7).
//!
//! Two rules govern every type and method here:
//!
//! 1. **Idempotent restatement.** Re-ingesting a `(tenant, platform, campaign, date)` row
//!    replaces its values; it never adds to them. `MetricsRepository::upsert_daily` is the
//!    one place this property must hold.
//! 2. **Two ROAS numbers, never blended.** `conversion_value` (the platform's own
//!    attribution) and `sanvi_revenue` (Sanvi's own order-based view) are carried side by
//!    side through every type below. Nothing in this module ever sums them into one field.

use async_trait::async_trait;
use chrono::NaiveDate;
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};

use super::model::{CampaignId, ConnectionId};
use super::{AdvertisingError, Money, PlatformKey};
use sanvi_persistence::ScopedTx;

/// One connection's reported metrics for one campaign on one day. This is
/// what `IngestConnectionMetrics` writes; `sanvi_revenue` is deliberately
/// absent — it is owned by the separate revenue join so a metrics-only
/// re-ingest can never clobber an already-attributed value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsDailyRow {
    pub tenant_id: TenantId,
    pub platform: PlatformKey,
    pub connection_id: ConnectionId,
    pub campaign_id: CampaignId,
    pub date: NaiveDate,
    pub impressions: i64,
    pub clicks: i64,
    pub spend: Money,
    /// Platform-reported conversions. Fractional: platforms weight
    /// view-through and cross-device conversions rather than always
    /// counting whole units.
    pub conversions: f64,
    pub conversion_value: Money,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricsGroupBy {
    Campaign,
    Platform,
    Tenant,
}

/// One row of a grouped query result. Which of `platform`/`campaign_id` are
/// populated depends on `group_by`: `Tenant` grain leaves both `None` and
/// still keeps rows split by `currency`, because sums across currencies do
/// not exist (NFR-1009).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricsAggregateRow {
    pub platform: Option<PlatformKey>,
    pub campaign_id: Option<CampaignId>,
    pub date: NaiveDate,
    pub currency: String,
    pub impressions: i64,
    pub clicks: i64,
    pub spend_minor: i64,
    pub conversions: f64,
    pub conversion_value_minor: i64,
    pub sanvi_revenue_minor: i64,
    /// `true` when `date` is still inside the platform's restatement window
    /// — the platform may still revise this day's numbers.
    pub restating: bool,
}

impl MetricsAggregateRow {
    /// `sanvi_revenue / spend`, `None` (never `∞`) on zero spend.
    pub fn roas_sanvi(&self) -> Option<f64> {
        roas(self.sanvi_revenue_minor, self.spend_minor)
    }

    /// `platform_conversion_value / spend`, `None` (never `∞`) on zero spend.
    pub fn roas_platform(&self) -> Option<f64> {
        roas(self.conversion_value_minor, self.spend_minor)
    }
}

fn roas(revenue_minor: i64, spend_minor: i64) -> Option<f64> {
    if spend_minor == 0 {
        None
    } else {
        Some(revenue_minor as f64 / spend_minor as f64)
    }
}

/// A bounded query over the rollup. `from`/`to` are inclusive and already
/// validated against the configured max span before reaching the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsRangeQuery {
    pub tenant_id: TenantId,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub group_by: MetricsGroupBy,
    pub platform: Option<PlatformKey>,
    pub campaign_id: Option<CampaignId>,
}

/// Per-connection ingestion staleness, derived from
/// `AdvertisingConnection::last_synced_at` — not a separate stored value, so
/// there is nothing here that can drift from what actually happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionFreshness {
    pub connection_id: ConnectionId,
    pub platform: PlatformKey,
    pub last_ingested_at: Option<chrono::DateTime<chrono::Utc>>,
    pub lag_hours: Option<f64>,
    pub stalled: bool,
}

#[async_trait]
pub trait MetricsRepository: Send + Sync {
    /// Idempotent replace, keyed by `(tenant_id, platform, campaign_id, date)`.
    /// Never touches `sanvi_revenue_minor` — that column is owned by
    /// `set_campaign_revenue`/`set_unattributed_revenue` so a later,
    /// metrics-only re-ingest of the same day can never erase an already
    /// attributed revenue figure.
    async fn upsert_daily(
        &self,
        scope: &mut ScopedTx<'_>,
        rows: &[MetricsDailyRow],
    ) -> Result<(), AdvertisingError>;

    /// Deletes every row for `connection_id` within `[from, to]` (inclusive)
    /// and then writes `rows` — the atomic "this fetch is now authoritative
    /// for this window" operation ingestion uses. A day/campaign a
    /// restatement no longer reports (paused, deleted, or now reporting
    /// nothing) is deleted rather than left behind with stale non-zero
    /// values; `rows` need not cover every day in the window. Same
    /// `sanvi_revenue_minor` caveat as `upsert_daily`: deleted rows lose
    /// any previously joined revenue, which is why the sweep always runs
    /// the revenue join again immediately after, over the same window.
    #[allow(clippy::too_many_arguments)]
    async fn replace_window(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        connection_id: ConnectionId,
        from: NaiveDate,
        to: NaiveDate,
        rows: &[MetricsDailyRow],
    ) -> Result<(), AdvertisingError>;

    /// Idempotent replace of one campaign-day's Sanvi-attributed revenue.
    /// A no-op if the target `metrics_daily` row does not exist yet — the
    /// revenue join always runs after ingestion for the same window, so in
    /// practice the row is already there.
    #[allow(clippy::too_many_arguments)]
    async fn set_campaign_revenue(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        campaign_id: CampaignId,
        date: NaiveDate,
        revenue_minor: i64,
    ) -> Result<(), AdvertisingError>;

    /// Idempotent replace of one tenant/platform/day's revenue that could
    /// not be attributed to a single campaign. Surfaced only in
    /// platform/tenant-grain queries — never distributed into a campaign row.
    async fn set_unattributed_revenue(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        date: NaiveDate,
        revenue_minor: i64,
        currency: &str,
    ) -> Result<(), AdvertisingError>;

    /// Deletes one tenant/platform/day's unattributed-revenue row, if any.
    /// Part of the revenue join's clear-then-rebuild pass: a day that
    /// previously had ambiguous revenue but now has none (event retracted,
    /// or attribution resolved to a single campaign) must not leave a
    /// stale row behind.
    async fn clear_unattributed_revenue(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        date: NaiveDate,
    ) -> Result<(), AdvertisingError>;

    /// Grouped aggregate over the rollup, bounded by `query`'s date span.
    /// `restatement_days` marks rows still inside the platform's
    /// restatement window as `restating`.
    async fn query_range(
        &self,
        scope: &mut ScopedTx<'_>,
        query: &MetricsRangeQuery,
        today: NaiveDate,
        restatement_days: i64,
    ) -> Result<Vec<MetricsAggregateRow>, AdvertisingError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        spend_minor: i64,
        conversion_value_minor: i64,
        sanvi_revenue_minor: i64,
    ) -> MetricsAggregateRow {
        MetricsAggregateRow {
            platform: None,
            campaign_id: None,
            date: NaiveDate::from_ymd_opt(2026, 8, 24).unwrap(),
            currency: "JPY".to_string(),
            impressions: 0,
            clicks: 0,
            spend_minor,
            conversions: 0.0,
            conversion_value_minor,
            sanvi_revenue_minor,
            restating: false,
        }
    }

    #[test]
    fn zero_spend_is_null_never_infinity() {
        let zero_spend = row(0, 5_000, 5_000);
        assert_eq!(zero_spend.roas_sanvi(), None);
        assert_eq!(zero_spend.roas_platform(), None);
    }

    #[test]
    fn zero_revenue_with_nonzero_spend_is_a_real_zero_not_null() {
        let zero_revenue = row(10_000, 0, 0);
        assert_eq!(zero_revenue.roas_sanvi(), Some(0.0));
        assert_eq!(zero_revenue.roas_platform(), Some(0.0));
    }

    #[test]
    fn zero_spend_and_zero_revenue_is_still_null() {
        let both_zero = row(0, 0, 0);
        assert_eq!(both_zero.roas_sanvi(), None);
        assert_eq!(both_zero.roas_platform(), None);
    }

    /// ROAS is a same-currency, same-row ratio of two minor-unit integers —
    /// JPY's zero decimal places (unlike USD's two) never enters the
    /// arithmetic, because nothing here multiplies or divides by an assumed
    /// exponent before computing the ratio.
    #[test]
    fn roas_arithmetic_ignores_currency_decimal_exponent() {
        // JPY: 20,000 minor units of revenue over 10,000 minor units of spend.
        let jpy = row(10_000, 0, 20_000);
        assert_eq!(jpy.roas_sanvi(), Some(2.0));

        // The same 2:1 ratio expressed in a hypothetical two-decimal
        // currency's minor units (e.g. USD cents) computes identically —
        // the exponent difference is irrelevant to the ratio.
        let two_decimal = row(100_000, 0, 200_000);
        assert_eq!(two_decimal.roas_sanvi(), Some(2.0));
    }

    #[test]
    fn platform_and_sanvi_roas_never_blend_even_when_they_differ() {
        // Platform over-attributes relative to Sanvi's own order-based view
        // — a realistic case (view-through credit, cross-device claims).
        let row = row(10_000, 15_000, 8_000);
        assert_eq!(row.roas_platform(), Some(1.5));
        assert_eq!(row.roas_sanvi(), Some(0.8));
        assert_ne!(row.roas_platform(), row.roas_sanvi());
    }
}
