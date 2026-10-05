//! SQLx persistence for advertising budget caps and alerts (TASK-017, slice 10.8).

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::model::{
    AlertCondition, BudgetAlert, BudgetAlertId, BudgetCap, BudgetCapId, BudgetPeriod, CampaignId,
    DataFreshness, Money,
};
use crate::domain::{
    AdvertisingError, BudgetAlertRepository, BudgetCapRepository, GuardrailSuspensionLedger,
};

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

    async fn campaign_exists(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        campaign_id: CampaignId,
    ) -> Result<bool, AdvertisingError> {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM advertising.campaigns WHERE tenant_id = $1 AND id = $2)",
        )
        .bind(tenant_id.as_i64())
        .bind(campaign_id.as_i64())
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(exists)
    }

    async fn list_by_tenant_page(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<BudgetCap>, AdvertisingError> {
        let rows = sqlx::query_as::<_, BudgetCapRow>(
            "SELECT id, tenant_id, campaign_id, period, amount_minor, currency,
                    declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                    version, created_at, updated_at
             FROM advertising.budget_caps
             WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2)
             ORDER BY id DESC LIMIT $3",
        )
        .bind(tenant_id.as_i64())
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        rows.into_iter().map(TryInto::try_into).collect()
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
        expected_version: Option<i64>,
    ) -> Result<BudgetCap, AdvertisingError> {
        // Optimistic locking, two paths:
        // - Versioned writes (`Some`): the conflicting-row UPDATE applies
        //   only if the stored version still matches; a zero-row update is a
        //   lost race → `StaleRevision` (409).
        // - Versionless creates (`None`, valid only when no row exists — the
        //   application layer rejects version-less updates first): a pure
        //   INSERT that does nothing on conflict, so two concurrent creates
        //   cannot silently overwrite each other; the loser sees no row and
        //   gets `StaleRevision` instead of a quiet last-writer-wins.
        let campaign_id_i64 = cap.campaign_id.map(|id| id.as_i64());
        // A named version takes the guarded compare-and-swap path; a
        // versionless call is a create and takes the insert-only path, so a
        // concurrent create conflicts instead of overwriting.
        let guarded = expected_version.is_some();
        let row = match campaign_id_i64 {
            None => {
                if guarded {
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
                     WHERE advertising.budget_caps.version = $14
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
                    .bind(expected_version)
                    .fetch_optional(&mut **scope.tx_mut().as_inner())
                    .await
                    .map_err(storage)?
                    .ok_or(AdvertisingError::StaleRevision)?
                } else {
                    sqlx::query_as::<_, BudgetCapRow>(
                        "INSERT INTO advertising.budget_caps
                       (id, tenant_id, campaign_id, period, amount_minor, currency,
                        declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                        version, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                     ON CONFLICT (tenant_id, period) WHERE campaign_id IS NULL DO NOTHING
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
                    .fetch_optional(&mut **scope.tx_mut().as_inner())
                    .await
                    .map_err(storage)?
                    .ok_or(AdvertisingError::StaleRevision)?
                }
            }
            Some(cid) => {
                if guarded {
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
                     WHERE advertising.budget_caps.version = $14
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
                .bind(expected_version)
.fetch_optional(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(storage)?
                .ok_or(AdvertisingError::StaleRevision)?
                } else {
                    sqlx::query_as::<_, BudgetCapRow>(
                    "INSERT INTO advertising.budget_caps
                       (id, tenant_id, campaign_id, period, amount_minor, currency,
                        declared_fx_basis, fx_rate_date, auto_pause, auto_resume_on_rollover,
                        version, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                     ON CONFLICT (tenant_id, campaign_id, period) WHERE campaign_id IS NOT NULL DO NOTHING
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
.fetch_optional(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(storage)?
                .ok_or(AdvertisingError::StaleRevision)?
                }
            }
        };

        row.try_into()
    }
}

/// Durable suspension-notice state machine
/// (`advertising.guardrail_suspensions`).
///
/// `claim_notice` is one statement: it inserts the suspended state when the
/// tenant has no row — and answers `false`, because a first observation is
/// not a rollback and must not notify by mail — and on conflict updates the row only
/// when the recorded state is `serving` (an observed enabled→suspended
/// transition) or the recorded reason differs *and that suspension was
/// actually notified* (a reason change re-arms only a notice the tenant has
/// seen; a silently recorded suspension stays silent). The
/// `RETURNING (xmax = 0)` discriminator tells insert (row created, nothing
/// to announce) from update (transition): Postgres gives a freshly inserted
/// row a new xmax of 0, while an `ON CONFLICT … DO UPDATE` leaves the
/// pre-existing row's nonzero xmax. A repeat of the recorded, completed
/// reason leaves the row untouched and returns no row: the caller publishes
/// nothing. Running inside the caller's transaction makes the claim atomic
/// with the event publish it gates, so a crash or a lost race between
/// replicas can neither double-send nor silently swallow a notice.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxGuardrailSuspensionLedger;

impl SqlxGuardrailSuspensionLedger {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl GuardrailSuspensionLedger for SqlxGuardrailSuspensionLedger {
    async fn claim_notice(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        reason: &str,
    ) -> Result<bool, AdvertisingError> {
        let claimed = sqlx::query_as::<_, (i64, bool)>(
            "INSERT INTO advertising.guardrail_suspensions (tenant_id, state, reason, suspended_at, notified_at)
             VALUES ($1, 'suspended', $2, now(), NULL)
             ON CONFLICT (tenant_id) DO UPDATE SET
               state = 'suspended',
               reason = EXCLUDED.reason,
               suspended_at = EXCLUDED.suspended_at,
               notified_at = now()
             WHERE advertising.guardrail_suspensions.state = 'serving'
                OR (advertising.guardrail_suspensions.reason IS DISTINCT FROM EXCLUDED.reason
                    AND advertising.guardrail_suspensions.notified_at IS NOT NULL)
             RETURNING tenant_id, (xmax = 0) AS inserted",
        )
        .bind(tenant_id.as_i64())
        .bind(reason)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        // `inserted` is true only for a freshly created row: the tenant was
        // observed suspended with no serving history, which is recorded but
        // never announced. `false` means an existing row was updated — an
        // observed transition. `None` means the row was left untouched (a
        // repeat of the recorded reason).
        Ok(matches!(claimed, Some((_, false))))
    }

    async fn mark_serving(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_ids: &[TenantId],
    ) -> Result<(), AdvertisingError> {
        if tenant_ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = tenant_ids.iter().map(|t| t.as_i64()).collect();
        sqlx::query(
            "INSERT INTO advertising.guardrail_suspensions AS g (tenant_id, state, reason, suspended_at)
             SELECT id, 'serving', 'none', now() FROM unnest($1::bigint[]) AS t(id)
             ON CONFLICT (tenant_id) DO UPDATE SET state = 'serving'
             WHERE g.state IS DISTINCT FROM 'serving'",
        )
        .bind(&ids)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
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
        limit: u32,
        after_id: Option<i64>,
    ) -> Result<Vec<BudgetAlert>, AdvertisingError> {
        let rows = sqlx::query_as::<_, BudgetAlertRow>(
            "SELECT id, tenant_id, cap_id, campaign_id, period, period_start, threshold,
                    condition, spend_minor, cap_minor, currency, data_freshness,
                    auto_paused, acknowledged_at, acknowledged_by, created_at
             FROM advertising.budget_alerts
             WHERE tenant_id = $1
               AND ($2::bigint IS NULL OR campaign_id = $2)
               AND (NOT $3 OR acknowledged_at IS NULL)
               AND ($5::bigint IS NULL OR id < $5)
             ORDER BY id DESC
             LIMIT $4",
        )
        .bind(tenant_id.as_i64())
        .bind(campaign_id.map(|id| id.as_i64()))
        .bind(unacknowledged_only)
        .bind(i64::from(limit))
        .bind(after_id)
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
