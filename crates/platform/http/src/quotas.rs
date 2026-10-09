//! Durable commercial quotas: PostgreSQL is authoritative, Redis disposable.
use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaDimension {
    ApiRequests,
    TrackedConversions,
    StorageBytes,
    CustomDomains,
    Seats,
    NotificationSends,
}

impl QuotaDimension {
    pub const ALL: [Self; 6] = [
        Self::ApiRequests,
        Self::TrackedConversions,
        Self::StorageBytes,
        Self::CustomDomains,
        Self::Seats,
        Self::NotificationSends,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiRequests => "api_requests",
            Self::TrackedConversions => "tracked_conversions",
            Self::StorageBytes => "storage_bytes",
            Self::CustomDomains => "custom_domains",
            Self::Seats => "seats",
            Self::NotificationSends => "notification_sends",
        }
    }
    pub fn is_gauge(self) -> bool {
        matches!(self, Self::StorageBytes | Self::CustomDomains | Self::Seats)
    }
    fn period(self, month: NaiveDate) -> NaiveDate {
        if self.is_gauge() {
            NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()
        } else {
            month
        }
    }
}
impl std::fmt::Display for QuotaDimension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaPlan {
    pub tier: String,
    pub limits: BTreeMap<QuotaDimension, u64>,
    pub upgrade_path: String,
}
impl QuotaPlan {
    /// Requests: measured 494k/tenant/day / 6 * 30 days for Starter;
    /// Professional measured share, Enterprise five shares. Resource budgets
    /// are commercial policy, not hardware capacity (quota-counting.md).
    pub fn for_tier(tier: &str) -> Result<Self, QuotaError> {
        let (limits, upgrade) = match tier {
            "starter" => (
                [2_470_000, 10_000, 1_073_741_824, 1, 5, 10_000],
                "professional",
            ),
            "professional" => (
                [14_820_000, 100_000, 10_737_418_240, 10, 25, 100_000],
                "enterprise",
            ),
            "enterprise" => (
                [74_100_000, 1_000_000, 107_374_182_400, 100, 250, 1_000_000],
                "contact_sales",
            ),
            _ => return Err(QuotaError::InvalidPlan(tier.to_owned())),
        };
        Ok(Self {
            tier: tier.to_owned(),
            limits: QuotaDimension::ALL.into_iter().zip(limits).collect(),
            upgrade_path: format!("/api/v1/tenant/billing/checkout-session?plan={upgrade}"),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DimensionUsage {
    pub used: u64,
    pub limit: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TenantUsage {
    pub plan: String,
    pub period_start: DateTime<Utc>,
    pub period_end: DateTime<Utc>,
    pub dimensions: BTreeMap<QuotaDimension, DimensionUsage>,
}
#[derive(Debug, thiserror::Error)]
pub enum QuotaError {
    #[error("{dimension} quota exceeded ({used}/{limit}); upgrade via {upgrade_path}")]
    Exceeded {
        dimension: QuotaDimension,
        used: u64,
        limit: u64,
        upgrade_path: String,
    },
    #[error("invalid quota delta")]
    InvalidDelta,
    #[error("unknown quota plan: {0}")]
    InvalidPlan(String),
    #[error("quota accounting infrastructure failed: {0}")]
    Infrastructure(#[from] anyhow::Error),
}
impl From<sqlx::Error> for QuotaError {
    fn from(e: sqlx::Error) -> Self {
        Self::Infrastructure(e.into())
    }
}
#[async_trait]
pub trait QuotaPlanProvider: Send + Sync {
    async fn plan_for(&self, tenant: TenantId) -> Result<QuotaPlan, QuotaError>;
}
pub struct PgQuotaPlanProvider {
    tx: TxManager,
}
impl PgQuotaPlanProvider {
    pub fn new(tx: TxManager) -> Self {
        Self { tx }
    }
}
#[async_trait]
impl QuotaPlanProvider for PgQuotaPlanProvider {
    async fn plan_for(&self, tenant: TenantId) -> Result<QuotaPlan, QuotaError> {
        let mut tx = self.tx.begin_for(tenant.as_i64()).await?;
        let tier = sqlx::query_scalar!(
            r#"SELECT p.tier AS "tier!" FROM billing.subscriptions s JOIN billing.plans p ON p.id=s.plan_id
            WHERE s.tenant_id=$1 AND s.status IN ('active','trialing','past_due') AND s.collection_state <> 'grace_expired'"#,
            tenant.as_i64(),
        ).fetch_optional(&mut **tx.tx_mut().as_inner()).await?;
        let plan = QuotaPlan::for_tier(tier.as_deref().unwrap_or("starter"))?;
        tx.commit().await?;
        Ok(plan)
    }
}
// String ordering keeps BIGINT versions exact above Lua's 2^53 precision.
const MIRROR_LUA: &str = r#"
local old=redis.call('HGET',KEYS[1],'version')
local new=ARGV[1]
if not old or #new>#old or (#new==#old and new>=old) then
 redis.call('HSET',KEYS[1],'version',new,'used',ARGV[2])
 redis.call('EXPIRE',KEYS[1],3600)
end
return redis.call('HGET',KEYS[1],'used')
"#;
pub struct QuotaService {
    tx: TxManager,
    plans: Arc<dyn QuotaPlanProvider>,
    redis: Option<redis::aio::ConnectionManager>,
    namespace: String,
}
impl QuotaService {
    pub fn new(
        tx: TxManager,
        plans: Arc<dyn QuotaPlanProvider>,
        redis: Option<redis::aio::ConnectionManager>,
    ) -> Self {
        Self {
            tx,
            plans,
            redis,
            namespace: "sanvi:quota:v1".to_owned(),
        }
    }
    /// Tests supply a namespace from `sanvi_testsupport::test_stream`.
    pub fn with_cache_namespace(mut self, namespace: String) -> Self {
        self.namespace = namespace;
        self
    }
    pub fn tx_manager(&self) -> TxManager {
        self.tx.clone()
    }
    pub async fn plan_for(&self, tenant: TenantId) -> Result<QuotaPlan, QuotaError> {
        self.plans.plan_for(tenant).await
    }
    /// Producers share-lock this policy until their transaction commits.
    pub async fn configure_enforcement(
        &self,
        tenant: TenantId,
        enabled: bool,
    ) -> Result<(), QuotaError> {
        let plan = self.plan_for(tenant).await?;
        let limits = serde_json::to_value(&plan.limits).map_err(anyhow::Error::from)?;
        let mut tx = self.tx.begin_for(tenant.as_i64()).await?;
        sqlx::query!("INSERT INTO platform.quota_policies (tenant_id,enforce,limits,upgrade_path) VALUES ($1,$2,$3,$4)
            ON CONFLICT (tenant_id) DO UPDATE SET enforce=EXCLUDED.enforce,limits=EXCLUDED.limits,upgrade_path=EXCLUDED.upgrade_path",
            tenant.as_i64(), enabled, limits, plan.upgrade_path)
            .execute(&mut **tx.tx_mut().as_inner()).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn usage(&self, tenant: TenantId) -> Result<TenantUsage, QuotaError> {
        let plan = self.plan_for(tenant).await?;
        let (start, end) = current_period();
        let mut tx = self.tx.begin_for(tenant.as_i64()).await?;
        let rows = sqlx::query!(
            "SELECT dimension,period_start,used,version FROM platform.quota_counters
            WHERE tenant_id=$1 AND period_start IN ($2,DATE '1970-01-01')",
            tenant.as_i64(),
            start.date_naive(),
        )
        .fetch_all(&mut **tx.tx_mut().as_inner())
        .await?;
        tx.commit().await?;
        let mut dimensions = BTreeMap::new();
        for dimension in QuotaDimension::ALL {
            let period = dimension.period(start.date_naive());
            let row = rows
                .iter()
                .find(|r| r.dimension == dimension.as_str() && r.period_start == period);
            let (used, version) = row.map(|r| (r.used, r.version)).unwrap_or((0, 0));
            let used = self
                .read_projection(tenant, dimension, period, used, version)
                .await;
            dimensions.insert(
                dimension,
                DimensionUsage {
                    used,
                    limit: plan.limits[&dimension],
                },
            );
        }
        Ok(TenantUsage {
            plan: plan.tier,
            period_start: start,
            period_end: end,
            dimensions,
        })
    }
    /// Atomic reservation when enforced; otherwise unconditional counting.
    pub async fn record(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        delta: i64,
        enforce: bool,
    ) -> Result<DimensionUsage, QuotaError> {
        if delta < 0 && !dimension.is_gauge() {
            return Err(QuotaError::InvalidDelta);
        }
        self.mutate(tenant, dimension, delta, false, enforce).await
    }
    /// External gauge snapshots must serialize with their producer changes.
    pub async fn set(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        value: u64,
        enforce: bool,
    ) -> Result<DimensionUsage, QuotaError> {
        if !dimension.is_gauge() {
            return Err(QuotaError::InvalidDelta);
        }
        self.mutate(
            tenant,
            dimension,
            i64::try_from(value).map_err(|_| QuotaError::InvalidDelta)?,
            true,
            enforce,
        )
        .await
    }
    async fn mutate(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        value: i64,
        absolute: bool,
        enforce: bool,
    ) -> Result<DimensionUsage, QuotaError> {
        let plan = self.plan_for(tenant).await?;
        let limit = plan.limits[&dimension];
        let period = dimension.period(current_period().0.date_naive());
        let mut tx = self.tx.begin_for(tenant.as_i64()).await?;
        sqlx::query!("INSERT INTO platform.quota_counters (tenant_id,dimension,period_start,used,version) VALUES ($1,$2,$3,0,0) ON CONFLICT DO NOTHING",
            tenant.as_i64(), dimension.as_str(), period).execute(&mut **tx.tx_mut().as_inner()).await?;
        let used=sqlx::query_scalar!("SELECT used FROM platform.quota_counters WHERE tenant_id=$1 AND dimension=$2 AND period_start=$3 FOR UPDATE",
            tenant.as_i64(), dimension.as_str(), period).fetch_one(&mut **tx.tx_mut().as_inner()).await?;
        let next = if absolute {
            value
        } else {
            used.checked_add(value).ok_or(QuotaError::InvalidDelta)?
        };
        if next < 0 {
            return Err(QuotaError::InvalidDelta);
        }
        // A downgrade still permits releasing existing resources.
        if enforce && next as u64 > limit && next > used {
            return Err(QuotaError::Exceeded {
                dimension,
                used: next as u64,
                limit,
                upgrade_path: plan.upgrade_path,
            });
        }
        let version=sqlx::query_scalar!("UPDATE platform.quota_counters SET used=$4,version=version+1 WHERE tenant_id=$1 AND dimension=$2 AND period_start=$3 RETURNING version",
            tenant.as_i64(), dimension.as_str(), period, next).fetch_one(&mut **tx.tx_mut().as_inner()).await?;
        tx.commit().await?;
        self.mirror(tenant, dimension, period, next, version).await;
        Ok(DimensionUsage {
            used: next as u64,
            limit,
        })
    }
    pub async fn enforce(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
    ) -> Result<(), QuotaError> {
        let usage = self.usage(tenant).await?;
        let current = &usage.dimensions[&dimension];
        if current.used > current.limit {
            return Err(QuotaError::Exceeded {
                dimension,
                used: current.used,
                limit: current.limit,
                upgrade_path: self.plan_for(tenant).await?.upgrade_path,
            });
        }
        Ok(())
    }
    async fn mirror(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        period: NaiveDate,
        used: i64,
        version: i64,
    ) {
        let Some(mut conn) = self.redis.clone() else {
            return;
        };
        let key = format!(
            "{}:{}:{}:{}",
            self.namespace,
            tenant.as_i64(),
            dimension,
            period
        );
        let result: redis::RedisResult<String> = redis::Script::new(MIRROR_LUA)
            .key(key)
            .arg(version.to_string())
            .arg(used.to_string())
            .invoke_async(&mut conn)
            .await;
        if let Err(error) = result {
            tracing::warn!(%error,"quota Redis projection unavailable; PostgreSQL remains authoritative");
        }
    }

    /// Redis is a read fast path only behind a PostgreSQL durability fence.
    /// Comparing both fields also rejects corrupted values with a plausible
    /// version. A miss is reconciled with the same committed PG snapshot.
    async fn read_projection(
        &self,
        tenant: TenantId,
        dimension: QuotaDimension,
        period: NaiveDate,
        used: i64,
        version: i64,
    ) -> u64 {
        if let Some(mut conn) = self.redis.clone() {
            let key = format!(
                "{}:{}:{}:{}",
                self.namespace,
                tenant.as_i64(),
                dimension,
                period
            );
            let cached: redis::RedisResult<(Option<String>, Option<String>)> = redis::cmd("HMGET")
                .arg(key)
                .arg("version")
                .arg("used")
                .query_async(&mut conn)
                .await;
            if let Ok((Some(cached_version), Some(cached_used))) = cached
                && cached_version.parse::<i64>() == Ok(version)
                && let Ok(cached_used) = cached_used.parse::<u64>()
                && cached_used == used as u64
            {
                return cached_used;
            }
        }
        self.mirror(tenant, dimension, period, used, version).await;
        used as u64
    }
}
fn current_period() -> (DateTime<Utc>, DateTime<Utc>) {
    let now = Utc::now();
    let start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .unwrap();
    let (year, month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    (
        start,
        Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn quota_redis_projection_rejects_delayed_writes_and_keeps_bigint_precision() {
        let client = redis::Client::open(sanvi_testsupport::redis_url().await).unwrap();
        let mut redis = redis::aio::ConnectionManager::new(client).await.unwrap();
        let key = sanvi_testsupport::test_stream("quota-version-order");
        for (version, used, want) in [
            ("9007199254740993", "20", "20"),
            ("9007199254740992", "999", "20"),
            ("9007199254740994", "10", "10"),
            ("9007199254740993", "20", "10"),
        ] {
            let actual: String = redis::Script::new(MIRROR_LUA)
                .key(&key)
                .arg(version)
                .arg(used)
                .invoke_async(&mut redis)
                .await
                .unwrap();
            assert_eq!(actual, want);
        }
        let _: i64 = redis::cmd("DEL")
            .arg(key)
            .query_async(&mut redis)
            .await
            .unwrap();
    }
}
