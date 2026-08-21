//! SQLx persistence for the payments context.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use sanvi_persistence::ScopedTx;

use crate::domain::connection::ProviderConnection;
use crate::domain::error::PaymentsError;
use crate::domain::ports::ProviderConnectionRepository;
use crate::domain::provider::{ConnectionStatus, ProviderKind};

fn storage(err: sqlx::Error) -> PaymentsError {
    PaymentsError::Storage(err.to_string())
}

#[derive(sqlx::FromRow)]
struct ProviderConnectionRow {
    id: Uuid,
    tenant_id: Uuid,
    provider: String,
    external_account_id: String,
    status: String,
    capabilities: Value,
    requirements: Value,
    country: Option<String>,
    default_currency: Option<String>,
    connected_at: Option<DateTime<Utc>>,
    last_synced_at: Option<DateTime<Utc>>,
    disconnected_at: Option<DateTime<Utc>>,
}

impl TryFrom<ProviderConnectionRow> for ProviderConnection {
    type Error = PaymentsError;

    fn try_from(row: ProviderConnectionRow) -> Result<Self, Self::Error> {
        let provider = ProviderKind::parse(&row.provider).ok_or_else(|| {
            PaymentsError::Storage(format!("unknown provider kind {:?}", row.provider))
        })?;
        let status = ConnectionStatus::parse(&row.status)
            .ok_or_else(|| PaymentsError::Storage(format!("unknown status {:?}", row.status)))?;
        Ok(ProviderConnection {
            id: row.id,
            tenant_id: row.tenant_id,
            provider,
            external_account_id: row.external_account_id,
            status,
            capabilities: row.capabilities,
            requirements: row.requirements,
            country: row.country,
            default_currency: row.default_currency,
            connected_at: row.connected_at,
            last_synced_at: row.last_synced_at,
            disconnected_at: row.disconnected_at,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxProviderConnectionRepository;

impl SqlxProviderConnectionRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ProviderConnectionRepository for SqlxProviderConnectionRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError> {
        sqlx::query!(
            r#"INSERT INTO payments.provider_connections
               (id, tenant_id, provider, external_account_id, status,
                capabilities, requirements, country, default_currency,
                connected_at, last_synced_at, disconnected_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"#,
            connection.id,
            connection.tenant_id,
            connection.provider.as_str(),
            connection.external_account_id,
            connection.status.as_str(),
            connection.capabilities,
            connection.requirements,
            connection.country,
            connection.default_currency,
            connection.connected_at,
            connection.last_synced_at,
            connection.disconnected_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| {
            // The partial unique index is the backstop behind the use
            // case's guard: a concurrent different-key create loses here
            // with a 409-shaped domain error, never a raw 500.
            if let sqlx::Error::Database(db) = &err
                && db.constraint() == Some("one_active_provider_per_tenant")
            {
                return PaymentsError::ConnectionExists(
                    "an active connection already exists".to_string(),
                );
            }
            storage(err)
        })?;
        Ok(())
    }

    async fn find_active(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        provider: &str,
    ) -> Result<Option<ProviderConnection>, PaymentsError> {
        let row = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE tenant_id = $1 AND provider = $2 AND disconnected_at IS NULL"#,
            tenant_id,
            provider,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<ProviderConnection>, PaymentsError> {
        let rows = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE tenant_id = $1
               ORDER BY connected_at DESC NULLS LAST, id DESC"#,
            tenant_id,
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(TryFrom::try_from).collect()
    }

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Option<ProviderConnection>, PaymentsError> {
        let row = sqlx::query_as!(
            ProviderConnectionRow,
            r#"SELECT id, tenant_id, provider, external_account_id, status,
                      capabilities, requirements, country, default_currency,
                      connected_at, last_synced_at, disconnected_at
               FROM payments.provider_connections
               WHERE tenant_id = $1 AND id = $2"#,
            tenant_id,
            id,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.map(TryFrom::try_from).transpose()
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &ProviderConnection,
    ) -> Result<(), PaymentsError> {
        sqlx::query!(
            r#"UPDATE payments.provider_connections
               SET external_account_id = $3, status = $4, capabilities = $5,
                   requirements = $6, country = $7, default_currency = $8,
                   connected_at = $9, last_synced_at = $10, disconnected_at = $11,
                   updated_at = now()
               WHERE id = $1 AND tenant_id = $2"#,
            connection.id,
            connection.tenant_id,
            connection.external_account_id,
            connection.status.as_str(),
            connection.capabilities,
            connection.requirements,
            connection.country,
            connection.default_currency,
            connection.connected_at,
            connection.last_synced_at,
            connection.disconnected_at,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }
}
