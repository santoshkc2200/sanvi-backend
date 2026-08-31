//! SQLx persistence for the metrics rollup and its ROAS query layer
//! (TASK-016, slice 10.7).

use async_trait::async_trait;
use chrono::NaiveDate;
use sanvi_kernel::TenantId;
use sanvi_persistence::ScopedTx;

use crate::domain::model::{CampaignId, ConnectionId};
use crate::domain::{
    AdvertisingError, MetricsAggregateRow, MetricsDailyRow, MetricsGroupBy, MetricsRangeQuery,
    MetricsRepository, PlatformKey,
};

fn storage(error: sqlx::Error) -> AdvertisingError {
    AdvertisingError::Storage(error.to_string())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxMetricsRepository;

impl SqlxMetricsRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MetricsRepository for SqlxMetricsRepository {
    async fn upsert_daily(
        &self,
        scope: &mut ScopedTx<'_>,
        rows: &[MetricsDailyRow],
    ) -> Result<(), AdvertisingError> {
        for row in rows {
            sqlx::query(
                "INSERT INTO advertising.metrics_daily
                   (tenant_id, platform, connection_id, campaign_id, date, impressions, clicks,
                    spend_minor, currency, conversions, conversion_value_minor, ingested_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::numeric,$11,now())
                 ON CONFLICT (tenant_id, platform, campaign_id, date) DO UPDATE SET
                   connection_id = EXCLUDED.connection_id,
                   impressions = EXCLUDED.impressions,
                   clicks = EXCLUDED.clicks,
                   spend_minor = EXCLUDED.spend_minor,
                   currency = EXCLUDED.currency,
                   conversions = EXCLUDED.conversions,
                   conversion_value_minor = EXCLUDED.conversion_value_minor,
                   ingested_at = now()",
            )
            .bind(row.tenant_id.as_i64())
            .bind(row.platform.as_str())
            .bind(row.connection_id.as_i64())
            .bind(row.campaign_id.as_i64())
            .bind(row.date)
            .bind(row.impressions)
            .bind(row.clicks)
            .bind(row.spend.amount_minor)
            .bind(&row.spend.currency)
            .bind(row.conversions)
            .bind(row.conversion_value.amount_minor)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?;
        }
        Ok(())
    }

    async fn replace_window(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        connection_id: ConnectionId,
        from: NaiveDate,
        to: NaiveDate,
        rows: &[MetricsDailyRow],
    ) -> Result<(), AdvertisingError> {
        sqlx::query(
            "DELETE FROM advertising.metrics_daily
             WHERE tenant_id = $1 AND platform = $2 AND connection_id = $3
               AND date BETWEEN $4 AND $5",
        )
        .bind(tenant_id.as_i64())
        .bind(platform.as_str())
        .bind(connection_id.as_i64())
        .bind(from)
        .bind(to)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        self.upsert_daily(scope, rows).await
    }

    async fn set_campaign_revenue(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        campaign_id: CampaignId,
        date: NaiveDate,
        revenue_minor: i64,
    ) -> Result<(), AdvertisingError> {
        sqlx::query(
            "UPDATE advertising.metrics_daily SET sanvi_revenue_minor = $1
             WHERE tenant_id = $2 AND platform = $3 AND campaign_id = $4 AND date = $5",
        )
        .bind(revenue_minor)
        .bind(tenant_id.as_i64())
        .bind(platform.as_str())
        .bind(campaign_id.as_i64())
        .bind(date)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn set_unattributed_revenue(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        date: NaiveDate,
        revenue_minor: i64,
        currency: &str,
    ) -> Result<(), AdvertisingError> {
        sqlx::query(
            "INSERT INTO advertising.metrics_unattributed_revenue_daily
               (tenant_id, platform, date, currency, revenue_minor, updated_at)
             VALUES ($1,$2,$3,$4,$5,now())
             ON CONFLICT (tenant_id, platform, date, currency) DO UPDATE SET
               revenue_minor = EXCLUDED.revenue_minor,
               updated_at = now()",
        )
        .bind(tenant_id.as_i64())
        .bind(platform.as_str())
        .bind(date)
        .bind(currency)
        .bind(revenue_minor)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn clear_unattributed_revenue(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        platform: &PlatformKey,
        date: NaiveDate,
    ) -> Result<(), AdvertisingError> {
        sqlx::query(
            "DELETE FROM advertising.metrics_unattributed_revenue_daily
             WHERE tenant_id = $1 AND platform = $2 AND date = $3",
        )
        .bind(tenant_id.as_i64())
        .bind(platform.as_str())
        .bind(date)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn query_range(
        &self,
        scope: &mut ScopedTx<'_>,
        query: &MetricsRangeQuery,
        today: NaiveDate,
        restatement_days: i64,
    ) -> Result<Vec<MetricsAggregateRow>, AdvertisingError> {
        let platform_filter = query.platform.as_ref().map(PlatformKey::as_str);
        let campaign_filter = query.campaign_id.map(|id| id.as_i64());

        let rows = match query.group_by {
            MetricsGroupBy::Campaign => sqlx::query_as::<_, CampaignGrainRow>(
                "SELECT platform, campaign_id, date, currency,
                            impressions, clicks, spend_minor, conversions::float8 AS conversions,
                            conversion_value_minor, sanvi_revenue_minor
                     FROM advertising.metrics_daily
                     WHERE tenant_id = $1 AND date BETWEEN $2 AND $3
                       AND ($4::text IS NULL OR platform = $4)
                       AND ($5::bigint IS NULL OR campaign_id = $5)
                     ORDER BY date ASC, campaign_id ASC",
            )
            .bind(query.tenant_id.as_i64())
            .bind(query.from)
            .bind(query.to)
            .bind(platform_filter)
            .bind(campaign_filter)
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?
            .into_iter()
            .map(CampaignGrainRow::into_aggregate)
            .collect::<Result<Vec<_>, _>>()?,
            MetricsGroupBy::Platform => sqlx::query_as::<_, GroupedRow>(
                "SELECT platform, date, currency,
                            SUM(impressions)::bigint AS impressions, SUM(clicks)::bigint AS clicks,
                            SUM(spend_minor)::bigint AS spend_minor,
                            SUM(conversions)::float8 AS conversions,
                            SUM(conversion_value_minor)::bigint AS conversion_value_minor,
                            SUM(sanvi_revenue_minor)::bigint AS sanvi_revenue_minor
                     FROM advertising.metrics_daily
                     WHERE tenant_id = $1 AND date BETWEEN $2 AND $3
                       AND ($4::text IS NULL OR platform = $4)
                     GROUP BY platform, date, currency
                     ORDER BY date ASC, platform ASC",
            )
            .bind(query.tenant_id.as_i64())
            .bind(query.from)
            .bind(query.to)
            .bind(platform_filter)
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?
            .into_iter()
            .map(GroupedRow::into_platform_aggregate)
            .collect::<Result<Vec<_>, _>>()?,
            MetricsGroupBy::Tenant => sqlx::query_as::<_, GroupedRow>(
                "SELECT NULL::text AS platform, date, currency,
                            SUM(impressions)::bigint AS impressions, SUM(clicks)::bigint AS clicks,
                            SUM(spend_minor)::bigint AS spend_minor,
                            SUM(conversions)::float8 AS conversions,
                            SUM(conversion_value_minor)::bigint AS conversion_value_minor,
                            SUM(sanvi_revenue_minor)::bigint AS sanvi_revenue_minor
                     FROM advertising.metrics_daily
                     WHERE tenant_id = $1 AND date BETWEEN $2 AND $3
                       AND ($4::text IS NULL OR platform = $4)
                     GROUP BY date, currency
                     ORDER BY date ASC",
            )
            .bind(query.tenant_id.as_i64())
            .bind(query.from)
            .bind(query.to)
            .bind(platform_filter)
            .fetch_all(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?
            .into_iter()
            .map(GroupedRow::into_platform_aggregate)
            .collect::<Result<Vec<_>, _>>()?,
        };

        // Unattributed revenue is added on top of the campaign-attributed
        // sum for platform/tenant grain only — never distributed into a
        // campaign row. Fetched separately and merged in Rust: the amounts
        // involved are tiny (one row per tenant/platform/day) and merging
        // here keeps the campaign-grain query free of an unrelated join.
        let rows = if matches!(
            query.group_by,
            MetricsGroupBy::Platform | MetricsGroupBy::Tenant
        ) {
            self.merge_unattributed(scope, query, rows).await?
        } else {
            rows
        };

        Ok(rows
            .into_iter()
            .map(|mut row| {
                row.restating = today.signed_duration_since(row.date).num_days() < restatement_days
                    && row.date <= today;
                row
            })
            .collect())
    }
}

impl SqlxMetricsRepository {
    async fn merge_unattributed(
        &self,
        scope: &mut ScopedTx<'_>,
        query: &MetricsRangeQuery,
        mut rows: Vec<MetricsAggregateRow>,
    ) -> Result<Vec<MetricsAggregateRow>, AdvertisingError> {
        let platform_filter = query.platform.as_ref().map(PlatformKey::as_str);
        let unattributed = sqlx::query_as::<_, UnattributedRow>(
            "SELECT platform, date, currency, revenue_minor
             FROM advertising.metrics_unattributed_revenue_daily
             WHERE tenant_id = $1 AND date BETWEEN $2 AND $3
               AND ($4::text IS NULL OR platform = $4)",
        )
        .bind(query.tenant_id.as_i64())
        .bind(query.from)
        .bind(query.to)
        .bind(platform_filter)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        for extra in unattributed {
            let platform = PlatformKey::parse(&extra.platform).map_err(|_| {
                AdvertisingError::Storage(format!(
                    "unknown advertising platform {:?}",
                    extra.platform
                ))
            })?;
            let key_platform =
                matches!(query.group_by, MetricsGroupBy::Platform).then_some(platform);
            let target = rows.iter_mut().find(|row| {
                row.date == extra.date
                    && row.currency == extra.currency
                    && row.platform == key_platform
            });
            match target {
                Some(row) => row.sanvi_revenue_minor += extra.revenue_minor,
                None => rows.push(MetricsAggregateRow {
                    platform: key_platform,
                    campaign_id: None,
                    date: extra.date,
                    currency: extra.currency,
                    impressions: 0,
                    clicks: 0,
                    spend_minor: 0,
                    conversions: 0.0,
                    conversion_value_minor: 0,
                    sanvi_revenue_minor: extra.revenue_minor,
                    restating: false,
                }),
            }
        }
        Ok(rows)
    }
}

#[derive(sqlx::FromRow)]
struct CampaignGrainRow {
    platform: String,
    campaign_id: i64,
    date: NaiveDate,
    currency: String,
    impressions: i64,
    clicks: i64,
    spend_minor: i64,
    conversions: f64,
    conversion_value_minor: i64,
    sanvi_revenue_minor: i64,
}

impl CampaignGrainRow {
    fn into_aggregate(self) -> Result<MetricsAggregateRow, AdvertisingError> {
        Ok(MetricsAggregateRow {
            platform: Some(PlatformKey::parse(&self.platform).map_err(|_| {
                AdvertisingError::Storage(format!(
                    "unknown advertising platform {:?}",
                    self.platform
                ))
            })?),
            campaign_id: Some(
                CampaignId::try_from(self.campaign_id)
                    .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
            ),
            date: self.date,
            currency: self.currency,
            impressions: self.impressions,
            clicks: self.clicks,
            spend_minor: self.spend_minor,
            conversions: self.conversions,
            conversion_value_minor: self.conversion_value_minor,
            sanvi_revenue_minor: self.sanvi_revenue_minor,
            restating: false,
        })
    }
}

#[derive(sqlx::FromRow)]
struct GroupedRow {
    platform: Option<String>,
    date: NaiveDate,
    currency: String,
    impressions: i64,
    clicks: i64,
    spend_minor: i64,
    conversions: f64,
    conversion_value_minor: i64,
    sanvi_revenue_minor: i64,
}

impl GroupedRow {
    fn into_platform_aggregate(self) -> Result<MetricsAggregateRow, AdvertisingError> {
        let platform = self
            .platform
            .map(|value| {
                PlatformKey::parse(&value).map_err(|_| {
                    AdvertisingError::Storage(format!("unknown advertising platform {value:?}"))
                })
            })
            .transpose()?;
        Ok(MetricsAggregateRow {
            platform,
            campaign_id: None,
            date: self.date,
            currency: self.currency,
            impressions: self.impressions,
            clicks: self.clicks,
            spend_minor: self.spend_minor,
            conversions: self.conversions,
            conversion_value_minor: self.conversion_value_minor,
            sanvi_revenue_minor: self.sanvi_revenue_minor,
            restating: false,
        })
    }
}

#[derive(sqlx::FromRow)]
struct UnattributedRow {
    platform: String,
    date: NaiveDate,
    currency: String,
    revenue_minor: i64,
}
