//! Tenant-scoped settings storage (`tenancy.tenant_settings`).
//!
//! Every method requires `&mut ScopedTx`: the RLS context (`SET LOCAL
//! sanvi.tenant_id`) is in force before this repository can even be reached,
//! so tenant isolation is guaranteed twice over (type system + Postgres).

use async_trait::async_trait;
use sanvi_persistence::ScopedTx;
use serde_json::Value;
use sqlx::FromRow;

use crate::domain::tenant::TenantId;
use crate::domain::{TenancyError, TenantSettingsRepository};

/// Row shape of `tenancy.tenant_settings`.
#[derive(Debug, Clone, FromRow)]
struct SettingRow {
    key: String,
    value: Value,
}

/// Tenant-scoped repository; methods demand the scoped handle.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxTenantSettingsRepository;

impl SqlxTenantSettingsRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TenantSettingsRepository for SqlxTenantSettingsRepository {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        key: &str,
    ) -> Result<Option<Value>, TenancyError> {
        let row = sqlx::query_as!(
            SettingRow,
            "SELECT key, value FROM tenancy.tenant_settings WHERE tenant_id = $1 AND key = $2",
            tenant_id.as_i64(),
            key
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        Ok(row.map(|row| row.value))
    }

    async fn set(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        key: &str,
        value: Value,
    ) -> Result<(), TenancyError> {
        sqlx::query!(
            "INSERT INTO tenancy.tenant_settings (tenant_id, key, value)
             VALUES ($1, $2, $3)
             ON CONFLICT (tenant_id, key) DO UPDATE
               SET value = EXCLUDED.value, updated_at = now()",
            tenant_id.as_i64(),
            key,
            value
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn list_all(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<(String, Value)>, TenancyError> {
        let rows = sqlx::query_as!(
            SettingRow,
            "SELECT key, value FROM tenancy.tenant_settings WHERE tenant_id = $1 ORDER BY key",
            tenant_id.as_i64()
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| TenancyError::Storage(err.to_string()))?;
        Ok(rows.into_iter().map(|row| (row.key, row.value)).collect())
    }
}
