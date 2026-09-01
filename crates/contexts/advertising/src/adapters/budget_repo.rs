//! SQLx persistence for advertising budget caps and alerts (TASK-017, slice 10.8).

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::model::{
    AlertCondition, BudgetAlert, BudgetAlertId, BudgetCap, BudgetCapId, BudgetPeriod, CampaignId,
    DataFreshness, Money,
};
use crate::domain::{AdvertisingError, BudgetAlertRepository, BudgetCapRepository};

fn storage(error: sqlx::Error) -> AdvertisingError {
    AdvertisingError::Storage(error.to_string())
}

#[derive(sqlx::FromRow)]
struct BudgetCapRow {
    id: i64,
    tenant_id: i64,
    campaign_id: Option<i64>,
    period: String,
    amount_minor: i64,
    currency: String,
    declared_fx_basis: Option<String>,
    fx_rate_date: Option<NaiveDate>,
    auto_pause: bool,
    auto_resume_on_rollover: bool,
    version: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<BudgetCapRow> for BudgetCap {
    type Error = AdvertisingError;

    fn try_from(row: BudgetCapRow) -> Result<Self, Self::Error> {
        let period = BudgetPeriod::parse(&row.period)?;
        let amount = Money::new(row.amount_minor, &row.currency)?;
        let campaign_id = row.campaign_id.map(CampaignId::try_from).transpose()?;
        Ok(Self {
            id: BudgetCapId::try_from(row.id)?,
            tenant_id: TenantId::try_from(row.tenant_id)?,
            campaign_id,
            period,
            amount,
            effective_currency: row.currency,
            declared_fx_basis: row.declared_fx_basis,
            fx_rate_date: row.fx_rate_date,
            auto_pause: row.auto_pause,
            auto_resume_on_rollover: row.auto_resume_on_rollover,
            version: row.version,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct BudgetAlertRow {
    id: i64,
    tenant_id: i64,
    cap_id: Option<i64>,
    campaign_id: Option<i64>,
    period: String,
    period_start: NaiveDate,
    threshold: i32,
    condition: String,
    spend_minor: i64,
    cap_minor: i64,
    currency: String,
    data_freshness: serde_json::Value,
    auto_paused: bool,
    acknowledged_at: Option<DateTime<Utc>>,
    acknowledged_by: Option<i64>,
    created_at: DateTime<Utc>,
}

impl TryFrom<BudgetAlertRow> for BudgetAlert {
    type Error = AdvertisingError;

    fn try_from(row: BudgetAlertRow) -> Result<Self, Self::Error> {
        let period = BudgetPeriod::parse(&row.period)?;
        let condition = AlertCondition::parse(&row.condition)?;
        let spend = Money::new(row.spend_minor, &row.currency)?;
        let cap_amount = Money::new(row.cap_minor, &row.currency)?;
        let cap_id = row.cap_id.map(BudgetCapId::try_from).transpose()?;
        let campaign_id = row.campaign_id.map(CampaignId::try_from).transpose()?;
        let acknowledged_by = row.acknowledged_by.map(UserId::try_from).transpose()?;
        let data_freshness: DataFreshness = serde_json::from_value(row.data_freshness)
            .map_err(|e| AdvertisingError::Storage(format!("invalid data freshness: {e}")))?;

        Ok(Self {
            id: BudgetAlertId::try_from(row.id)?,
            tenant_id: TenantId::try_from(row.tenant_id)?,
            cap_id,
            campaign_id,
            period,
            period_start: row.period_start,
            threshold: row.threshold as u32,
            condition,
            spend,
            cap_amount,
            data_freshness,
            auto_paused: row.auto_paused,
            acknowledged_at: row.acknowledged_at,
            acknowledged_by,
            created_at: row.created_at,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxBudgetCapRepository;

impl SqlxBudgetCapRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BudgetCapRepository for SqlxBudgetCapRepository {
    async fn get_tenant_cap(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        period: BudgetPeriod,
    ) -> Result<Option<BudgetCap>, AdvertisingError> {
        let row = sqlx::query_as::<_, BudgetCapRow>(
            "SELECT id, tenant_id, campaign_id, period, amount_minor, currency,
                    declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                    version, created_at, updated_at
             FROM advertising.budget_caps
             WHERE tenant_id = $1 AND campaign_id IS NULL AND period = $2",
        )
        .bind(tenant_id.as_i64())
        .bind(period.as_str())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        row.map(TryInto::try_into).transpose()
    }

    async fn get_campaign_cap(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        period: BudgetPeriod,
    ) -> Result<Option<BudgetCap>, AdvertisingError> {
        let row = sqlx::query_as::<_, BudgetCapRow>(
            "SELECT id, tenant_id, campaign_id, period, amount_minor, currency,
                    declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                    version, created_at, updated_at
             FROM advertising.budget_caps
             WHERE tenant_id = $1 AND campaign_id = $2 AND period = $3",
        )
        .bind(tenant_id.as_i64())
        .bind(campaign_id.as_i64())
        .bind(period.as_str())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        row.map(TryInto::try_into).transpose()
    }

    async fn list_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<BudgetCap>, AdvertisingError> {
        let rows = sqlx::query_as::<_, BudgetCapRow>(
            "SELECT id, tenant_id, campaign_id, period, amount_minor, currency,
                    declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                    version, created_at, updated_at
             FROM advertising.budget_caps
             WHERE tenant_id = $1
             ORDER BY campaign_id ASC NULLS FIRST, period ASC",
        )
        .bind(tenant_id.as_i64())
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn list_all_active_caps(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<BudgetCap>, AdvertisingError> {
        let rows = sqlx::query_as::<_, BudgetCapRow>(
            "SELECT id, tenant_id, campaign_id, period, amount_minor, currency,
                    declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                    version, created_at, updated_at
             FROM advertising.budget_caps
             ORDER BY tenant_id ASC, id ASC",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn upsert_cap(
        &self,
        scope: &mut ScopedTx<'_>,
        cap: &BudgetCap,
    ) -> Result<BudgetCap, AdvertisingError> {
        let campaign_id_i64 = cap.campaign_id.map(|id| id.as_i64());
        let row = match campaign_id_i64 {
            None => {
                sqlx::query_as::<_, BudgetCapRow>(
                    "INSERT INTO advertising.budget_caps
                       (id, tenant_id, campaign_id, period, amount_minor, currency,
                        declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                        version, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                     ON CONFLICT (tenant_id, period) WHERE campaign_id IS NULL DO UPDATE SET
                       amount_minor = EXCLUDED.amount_minor,
                       currency = EXCLUDED.currency,
                       declared_fx_basis = EXCLUDED.declared_fx_basis,
                       fx_rate_date = EXCLUDED.fx_rate_date,
                       auto_pause = EXCLUDED.auto_pause,
                       auto_resume_on_rollover = EXCLUDED.auto_resume_on_rollover,
                       version = advertising.budget_caps.version + 1,
                       updated_at = now()
                     RETURNING id, tenant_id, campaign_id, period, amount_minor, currency,
                       declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                       version, created_at, updated_at",
                )
                .bind(cap.id.as_i64())
                .bind(cap.tenant_id.as_i64())
                .bind(campaign_id_i64)
                .bind(cap.period.as_str())
                .bind(cap.amount.amount_minor)
                .bind(&cap.amount.currency)
                .bind(&cap.declared_fx_basis)
                .bind(cap.fx_rate_date)
                .bind(cap.auto_pause)
                .bind(cap.auto_resume_on_rollover)
                .bind(cap.version)
                .bind(cap.created_at)
                .bind(cap.updated_at)
                .fetch_one(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(storage)?
            }
            Some(cid) => {
                sqlx::query_as::<_, BudgetCapRow>(
                    "INSERT INTO advertising.budget_caps
                       (id, tenant_id, campaign_id, period, amount_minor, currency,
                        declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                        version, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                     ON CONFLICT (tenant_id, campaign_id, period) WHERE campaign_id IS NOT NULL DO UPDATE SET
                       amount_minor = EXCLUDED.amount_minor,
                       currency = EXCLUDED.currency,
                       declared_fx_basis = EXCLUDED.declared_fx_basis,
                       fx_rate_date = EXCLUDED.fx_rate_date,
                       auto_pause = EXCLUDED.auto_pause,
                       auto_resume_on_rollover = EXCLUDED.auto_resume_on_rollover,
                       version = advertising.budget_caps.version + 1,
                       updated_at = now()
                     RETURNING id, tenant_id, campaign_id, period, amount_minor, currency,
                       declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                       version, created_at, updated_at",
                )
                .bind(cap.id.as_i64())
                .bind(cap.tenant_id.as_i64())
                .bind(cid)
                .bind(cap.period.as_str())
                .bind(cap.amount.amount_minor)
                .bind(&cap.amount.currency)
                .bind(&cap.declared_fx_basis)
                .bind(cap.fx_rate_date)
                .bind(cap.auto_pause)
                .bind(cap.auto_resume_on_rollover)
                .bind(cap.version)
                .bind(cap.created_at)
                .bind(cap.updated_at)
                .fetch_one(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(storage)?
            }
        };

        row.try_into()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxBudgetAlertRepository;

impl SqlxBudgetAlertRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BudgetAlertRepository for SqlxBudgetAlertRepository {
    async fn list_alerts(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        campaign_id: Option<CampaignId>,
        unacknowledged_only: bool,
        limit: i64,
    ) -> Result<Vec<BudgetAlert>, AdvertisingError> {
        let rows = sqlx::query_as::<_, BudgetAlertRow>(
            "SELECT id, tenant_id, cap_id, campaign_id, period, period_start, threshold,
                    condition, spend_minor, cap_minor, currency, data_freshness,
                    auto_paused, acknowledged_at, acknowledged_by, created_at
             FROM advertising.budget_alerts
             WHERE tenant_id = $1
               AND ($2::bigint IS NULL OR campaign_id = $2)
               AND (NOT $3 OR acknowledged_at IS NULL)
             ORDER BY created_at DESC, id DESC
             LIMIT $4",
        )
        .bind(tenant_id.as_i64())
        .bind(campaign_id.map(|id| id.as_i64()))
        .bind(unacknowledged_only)
        .bind(limit)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: BudgetAlertId,
    ) -> Result<Option<BudgetAlert>, AdvertisingError> {
        let row = sqlx::query_as::<_, BudgetAlertRow>(
            "SELECT id, tenant_id, cap_id, campaign_id, period, period_start, threshold,
                    condition, spend_minor, cap_minor, currency, data_freshness,
                    auto_paused, acknowledged_at, acknowledged_by, created_at
             FROM advertising.budget_alerts
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id.as_i64())
        .bind(id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        row.map(TryInto::try_into).transpose()
    }

    async fn has_alert_fired(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        cap_id: BudgetCapId,
        period_start: NaiveDate,
        threshold: u32,
    ) -> Result<bool, AdvertisingError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM advertising.budget_alerts
             WHERE tenant_id = $1 AND cap_id = $2 AND period_start = $3 AND threshold = $4",
        )
        .bind(tenant_id.as_i64())
        .bind(cap_id.as_i64())
        .bind(period_start)
        .bind(threshold as i32)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        Ok(count > 0)
    }

    async fn record_alert(
        &self,
        scope: &mut ScopedTx<'_>,
        alert: &BudgetAlert,
    ) -> Result<BudgetAlert, AdvertisingError> {
        let freshness_json = serde_json::to_value(&alert.data_freshness)
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        let row = sqlx::query_as::<_, BudgetAlertRow>(
            "INSERT INTO advertising.budget_alerts
               (id, tenant_id, cap_id, campaign_id, period, period_start, threshold, condition,
                spend_minor, cap_minor, currency, data_freshness, auto_paused,
                acknowledged_at, acknowledged_by, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (tenant_id, cap_id, period_start, threshold) DO UPDATE SET
               spend_minor = EXCLUDED.spend_minor,
               data_freshness = EXCLUDED.data_freshness
             RETURNING id, tenant_id, cap_id, campaign_id, period, period_start, threshold, condition,
               spend_minor, cap_minor, currency, data_freshness, auto_paused,
               acknowledged_at, acknowledged_by, created_at",
        )
        .bind(alert.id.as_i64())
        .bind(alert.tenant_id.as_i64())
        .bind(alert.cap_id.map(|id| id.as_i64()))
        .bind(alert.campaign_id.map(|id| id.as_i64()))
        .bind(alert.period.as_str())
        .bind(alert.period_start)
        .bind(alert.threshold as i32)
        .bind(alert.condition.as_str())
        .bind(alert.spend.amount_minor)
        .bind(alert.cap_amount.amount_minor)
        .bind(&alert.spend.currency)
        .bind(freshness_json)
        .bind(alert.auto_paused)
        .bind(alert.acknowledged_at)
        .bind(alert.acknowledged_by.map(|u| u.as_i64()))
        .bind(alert.created_at)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        row.try_into()
    }

    async fn acknowledge_alert(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: BudgetAlertId,
        user_id: UserId,
        acknowledged_at: DateTime<Utc>,
    ) -> Result<Option<BudgetAlert>, AdvertisingError> {
        let row = sqlx::query_as::<_, BudgetAlertRow>(
            "UPDATE advertising.budget_alerts
             SET acknowledged_at = $3, acknowledged_by = $4
             WHERE tenant_id = $1 AND id = $2 AND acknowledged_at IS NULL
             RETURNING id, tenant_id, cap_id, campaign_id, period, period_start, threshold, condition,
               spend_minor, cap_minor, currency, data_freshness, auto_paused,
               acknowledged_at, acknowledged_by, created_at",
        )
        .bind(tenant_id.as_i64())
        .bind(id.as_i64())
        .bind(acknowledged_at)
        .bind(user_id.as_i64())
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        row.map(TryInto::try_into).transpose()
    }
}
