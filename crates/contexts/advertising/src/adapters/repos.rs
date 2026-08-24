//! SQLx persistence for tenant advertising connections.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::ScopedTx;
use uuid::Uuid;

use crate::domain::{
    AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError, ConnectionStatus,
    PlatformKey, SecretRef, SyncError,
};

#[derive(sqlx::FromRow)]
struct ConnectionRow {
    id: Uuid,
    tenant_id: Uuid,
    platform: String,
    external_account_id: String,
    account_name: Option<String>,
    currency: String,
    timezone: String,
    status: String,
    scopes: Vec<String>,
    token_ref: String,
    connected_by: Option<Uuid>,
    connected_at: DateTime<Utc>,
    token_expires_at: Option<DateTime<Utc>>,
    last_synced_at: Option<DateTime<Utc>>,
    last_error: Option<serde_json::Value>,
    disconnected_notified_at: Option<DateTime<Utc>>,
    dependent_jobs_paused_at: Option<DateTime<Utc>>,
}

impl TryFrom<ConnectionRow> for AdvertisingConnection {
    type Error = AdvertisingError;

    fn try_from(row: ConnectionRow) -> Result<Self, Self::Error> {
        let platform = PlatformKey::parse(&row.platform).map_err(|_| {
            AdvertisingError::Storage(format!("unknown advertising platform {:?}", row.platform))
        })?;
        let status = ConnectionStatus::parse(&row.status).ok_or_else(|| {
            AdvertisingError::Storage(format!("unknown advertising status {:?}", row.status))
        })?;
        let last_error = row
            .last_error
            .map(serde_json::from_value::<SyncError>)
            .transpose()
            .map_err(|error| {
                AdvertisingError::Storage(format!("invalid advertising sync error: {error}"))
            })?;
        AdvertisingConnection::from_persisted(
            row.id,
            row.tenant_id,
            platform,
            row.external_account_id,
            row.account_name,
            row.currency,
            row.timezone,
            status,
            row.scopes,
            SecretRef::new(row.token_ref),
            row.connected_by,
            row.connected_at,
            row.token_expires_at,
            row.last_synced_at,
            last_error,
            row.disconnected_notified_at,
            row.dependent_jobs_paused_at,
        )
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxAdvertisingConnectionRepository;

impl SqlxAdvertisingConnectionRepository {
    pub const fn new() -> Self {
        Self
    }
}

fn storage(error: sqlx::Error) -> AdvertisingError {
    AdvertisingError::Storage(error.to_string())
}

#[async_trait]
impl AdvertisingConnectionRepository for SqlxAdvertisingConnectionRepository {
    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &AdvertisingConnection,
    ) -> Result<AdvertisingConnection, AdvertisingError> {
        let row = sqlx::query_as::<_, ConnectionRow>(
            "INSERT INTO advertising.connections
             (id, tenant_id, platform, external_account_id, account_name, currency, timezone,
              status, scopes, token_ref, connected_by, connected_at, token_expires_at,
              last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
             ON CONFLICT (tenant_id, platform, external_account_id) DO UPDATE SET
               account_name = EXCLUDED.account_name, currency = EXCLUDED.currency,
               timezone = EXCLUDED.timezone, status = EXCLUDED.status, scopes = EXCLUDED.scopes,
               token_ref = EXCLUDED.token_ref, connected_by = EXCLUDED.connected_by,
               connected_at = EXCLUDED.connected_at, token_expires_at = EXCLUDED.token_expires_at,
               last_synced_at = EXCLUDED.last_synced_at, last_error = EXCLUDED.last_error,
               disconnected_notified_at = EXCLUDED.disconnected_notified_at,
               dependent_jobs_paused_at = EXCLUDED.dependent_jobs_paused_at,
               updated_at = now()
             RETURNING id, tenant_id, platform, external_account_id, account_name, currency,
               timezone, status, scopes, token_ref, connected_by, connected_at, token_expires_at,
               last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at",
        )
        .bind(connection.id())
        .bind(connection.tenant_id())
        .bind(connection.platform().as_str())
        .bind(connection.external_account_id())
        .bind(connection.account_name())
        .bind(connection.currency())
        .bind(connection.timezone())
        .bind(connection.status().as_str())
        .bind(connection.granted_scopes())
        .bind(connection.token_ref().persisted_locator())
        .bind(connection.connected_by())
        .bind(connection.connected_at())
        .bind(connection.token_expires_at())
        .bind(connection.last_synced_at())
        .bind(
            connection
                .last_error()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| {
                    AdvertisingError::Storage(format!(
                        "advertising sync error serialization failed: {error}"
                    ))
                })?,
        )
        .bind(connection.disconnected_notified_at())
        .bind(connection.dependent_jobs_paused_at())
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.try_into()
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<AdvertisingConnection>, AdvertisingError> {
        let rows = sqlx::query_as::<_, ConnectionRow>(
            "SELECT id, tenant_id, platform, external_account_id, account_name, currency,
               timezone, status, scopes, token_ref, connected_by, connected_at, token_expires_at,
               last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at
             FROM advertising.connections
             WHERE tenant_id = $1 ORDER BY connected_at DESC, id DESC",
        )
        .bind(tenant_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        connection_id: Uuid,
    ) -> Result<Option<AdvertisingConnection>, AdvertisingError> {
        let row = sqlx::query_as::<_, ConnectionRow>(
            "SELECT id, tenant_id, platform, external_account_id, account_name, currency,
               timezone, status, scopes, token_ref, connected_by, connected_at, token_expires_at,
               last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at
             FROM advertising.connections
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(connection_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryInto::try_into).transpose()
    }
}
