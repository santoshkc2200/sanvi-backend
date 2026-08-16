//! The sqlx entitlement and quota repositories (`access.entitlements`,
//! `access.quota_usage`). Tenant-scoped reads run under `ScopedTx`
//! (RLS-filtered by construction); writes and maintenance run under the
//! audited `PlatformScope`.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use sanvi_persistence::{PlatformScope, ScopedTx};
use sqlx::FromRow;
use uuid::Uuid;

use crate::domain::entitlements::{Entitlement, EntitlementSource};
use crate::domain::features::FeatureKey;
use crate::domain::{AccessError, EntitlementRepository, QuotaUsageStore};

#[derive(Debug, Clone, FromRow)]
struct EntitlementRow {
    tenant_id: Uuid,
    feature_key: String,
    enabled: bool,
    limit: Option<i64>,
    source: String,
    expires_at: Option<DateTime<Utc>>,
    granted_by: Option<Uuid>,
    granted_reason: Option<String>,
    updated_at: DateTime<Utc>,
}

fn to_entitlement(row: EntitlementRow) -> Result<Entitlement, AccessError> {
    Ok(Entitlement::from_parts(
        row.tenant_id,
        FeatureKey::parse(&row.feature_key)?,
        row.enabled,
        row.limit,
        EntitlementSource::parse(&row.source)?,
        row.expires_at,
        row.granted_by,
        row.granted_reason,
        row.updated_at,
    ))
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxEntitlementRepository;

impl SqlxEntitlementRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EntitlementRepository for SqlxEntitlementRepository {
    async fn list_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<Entitlement>, AccessError> {
        let rows: Vec<EntitlementRow> = sqlx::query_as(
            "SELECT tenant_id, feature_key, enabled, \"limit\", source,
                    expires_at, granted_by, granted_reason, updated_at
             FROM access.entitlements
             WHERE tenant_id = $1 ORDER BY feature_key, source",
        )
        .bind(tenant_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        rows.into_iter().map(to_entitlement).collect()
    }

    async fn list_for_tenant_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<Entitlement>, AccessError> {
        let rows: Vec<EntitlementRow> = sqlx::query_as(
            "SELECT tenant_id, feature_key, enabled, \"limit\", source,
                    expires_at, granted_by, granted_reason, updated_at
             FROM access.entitlements
             WHERE tenant_id = $1 ORDER BY feature_key, source",
        )
        .bind(tenant_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        rows.into_iter().map(to_entitlement).collect()
    }

    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        entitlement: &Entitlement,
    ) -> Result<(), AccessError> {
        sqlx::query(
            "INSERT INTO access.entitlements
               (tenant_id, feature_key, enabled, \"limit\", source, expires_at,
                granted_by, granted_reason, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (tenant_id, feature_key, source) DO UPDATE SET
               enabled = EXCLUDED.enabled,
               \"limit\" = EXCLUDED.\"limit\",
               expires_at = EXCLUDED.expires_at,
               granted_by = EXCLUDED.granted_by,
               granted_reason = EXCLUDED.granted_reason,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(entitlement.tenant_id())
        .bind(entitlement.feature().as_str())
        .bind(entitlement.enabled())
        .bind(entitlement.limit())
        .bind(entitlement.source().as_str())
        .bind(entitlement.expires_at())
        .bind(entitlement.granted_by())
        .bind(entitlement.granted_reason())
        .bind(entitlement.updated_at())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        feature: &FeatureKey,
        source: EntitlementSource,
    ) -> Result<(), AccessError> {
        sqlx::query(
            "DELETE FROM access.entitlements
             WHERE tenant_id = $1 AND feature_key = $2 AND source = $3",
        )
        .bind(tenant_id)
        .bind(feature.as_str())
        .bind(source.as_str())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn count_active(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<u32, AccessError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM access.entitlements
             WHERE tenant_id = $1 AND source = 'override'
               AND (expires_at IS NULL OR expires_at > $2)",
        )
        .bind(tenant_id)
        .bind(now)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(count as u32)
    }

    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Entitlement>, AccessError> {
        let rows: Vec<EntitlementRow> = sqlx::query_as(
            "SELECT tenant_id, feature_key, enabled, \"limit\", source,
                    expires_at, granted_by, granted_reason, updated_at
             FROM access.entitlements
             WHERE source = 'override' AND expires_at <= $1
             ORDER BY expires_at LIMIT $2",
        )
        .bind(now)
        .bind(i64::from(limit))
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        rows.into_iter().map(to_entitlement).collect()
    }

    async fn list_by_granted_by(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: Uuid,
    ) -> Result<Vec<Entitlement>, AccessError> {
        let rows: Vec<EntitlementRow> = sqlx::query_as(
            "SELECT tenant_id, feature_key, enabled, \"limit\", source,
                    expires_at, granted_by, granted_reason, updated_at
             FROM access.entitlements WHERE granted_by = $1",
        )
        .bind(user_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        rows.into_iter().map(to_entitlement).collect()
    }

    async fn anonymise_granted_by(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: Uuid,
    ) -> Result<usize, AccessError> {
        let result = sqlx::query(
            "UPDATE access.entitlements
             SET granted_by = NULL,
                 granted_reason = 'redacted by privacy erasure (phase 05)'
             WHERE granted_by = $1",
        )
        .bind(user_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(result.rows_affected() as usize)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxQuotaUsageStore;

impl SqlxQuotaUsageStore {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaUsageStore for SqlxQuotaUsageStore {
    async fn consume(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        feature: &FeatureKey,
        period: NaiveDate,
        amount: i64,
        limit: i64,
    ) -> Result<Option<i64>, AccessError> {
        // Atomic: the row update only succeeds while the new total stays
        // within the limit; a missing RETURNING row means "exceeded". Two
        // concurrent consumers cannot both take the last unit.
        let used: Option<i64> = sqlx::query_scalar(
            "INSERT INTO access.quota_usage (tenant_id, feature_key, period_start, used)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id, feature_key, period_start) DO UPDATE
               SET used = access.quota_usage.used + EXCLUDED.used
               WHERE access.quota_usage.used + EXCLUDED.used <= $5
             RETURNING used",
        )
        .bind(tenant_id)
        .bind(feature.as_str())
        .bind(period)
        .bind(amount)
        .bind(limit)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(used)
    }

    async fn usage(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        feature: &FeatureKey,
        period: NaiveDate,
    ) -> Result<i64, AccessError> {
        let used: Option<i64> = sqlx::query_scalar(
            "SELECT used FROM access.quota_usage
             WHERE tenant_id = $1 AND feature_key = $2 AND period_start = $3",
        )
        .bind(tenant_id)
        .bind(feature.as_str())
        .bind(period)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| AccessError::Storage(err.to_string()))?;
        Ok(used.unwrap_or(0))
    }

    async fn rollover(
        &self,
        scope: &mut PlatformScope<'_>,
        before: NaiveDate,
    ) -> Result<u64, AccessError> {
        let deleted: u64 = sqlx::query("DELETE FROM access.quota_usage WHERE period_start < $1")
            .bind(before)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?
            .rows_affected();
        Ok(deleted)
    }
}
