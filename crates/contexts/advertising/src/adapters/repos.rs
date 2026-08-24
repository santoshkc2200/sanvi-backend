//! SQLx persistence for tenant advertising connections.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::ScopedTx;
use uuid::Uuid;

use crate::domain::{
    AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError, CampaignChange,
    CampaignRepository, ChangeSource, ConnectionStatus, PlatformKey, SecretRef, StoredCampaign,
    SyncError,
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

#[derive(sqlx::FromRow)]
struct CampaignRow {
    id: Uuid,
    tenant_id: Uuid,
    connection_id: Uuid,
    platform: String,
    external_id: Option<String>,
    revision: i64,
    intent: serde_json::Value,
    remote_snapshot: serde_json::Value,
}

impl TryFrom<CampaignRow> for StoredCampaign {
    type Error = AdvertisingError;

    fn try_from(row: CampaignRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            connection_id: row.connection_id,
            platform: PlatformKey::parse(&row.platform).map_err(|_| {
                AdvertisingError::Storage(format!(
                    "unknown advertising platform {:?}",
                    row.platform
                ))
            })?,
            external_id: row.external_id,
            revision: row.revision,
            intent: serde_json::from_value(row.intent).map_err(|error| {
                AdvertisingError::Storage(format!("invalid stored campaign intent: {error}"))
            })?,
            remote_snapshot: serde_json::from_value(row.remote_snapshot).map_err(|error| {
                AdvertisingError::Storage(format!("invalid campaign remote snapshot: {error}"))
            })?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct CampaignChangeRow {
    id: Uuid,
    tenant_id: Uuid,
    campaign_id: Uuid,
    revision: i64,
    mutation_id: Option<Uuid>,
    actor_id: Option<Uuid>,
    source: String,
    changed_fields: serde_json::Value,
    before_state: serde_json::Value,
    after_state: serde_json::Value,
    created_at: DateTime<Utc>,
}

impl TryFrom<CampaignChangeRow> for CampaignChange {
    type Error = AdvertisingError;

    fn try_from(row: CampaignChangeRow) -> Result<Self, Self::Error> {
        let source = match row.source.as_str() {
            "sanvi" => ChangeSource::Sanvi,
            "platform" => ChangeSource::Platform,
            _ => {
                return Err(AdvertisingError::Storage(
                    "unknown campaign change source".to_string(),
                ));
            }
        };
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            campaign_id: row.campaign_id,
            revision: row.revision,
            mutation_id: row.mutation_id,
            actor_id: row.actor_id,
            source,
            changed_fields: serde_json::from_value(row.changed_fields).map_err(|error| {
                AdvertisingError::Storage(format!("invalid campaign change fields: {error}"))
            })?,
            before_state: serde_json::from_value(row.before_state).map_err(|error| {
                AdvertisingError::Storage(format!("invalid campaign change before state: {error}"))
            })?,
            after_state: serde_json::from_value(row.after_state).map_err(|error| {
                AdvertisingError::Storage(format!("invalid campaign change after state: {error}"))
            })?,
            created_at: row.created_at,
        })
    }
}

/// SQL persistence for campaign intent, last remote snapshot and mutation
/// audit entries.  The surrounding `ScopedTx` sets the tenant RLS value.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxCampaignRepository;

impl SqlxCampaignRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl CampaignRepository for SqlxCampaignRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        campaign: &StoredCampaign,
    ) -> Result<(), AdvertisingError> {
        sqlx::query(
            "INSERT INTO advertising.campaigns
             (id, tenant_id, connection_id, platform, external_id, name, objective, status, budget,
              schedule, drift, revision, intent, remote_snapshot)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
        )
        .bind(campaign.id)
        .bind(campaign.tenant_id)
        .bind(campaign.connection_id)
        .bind(campaign.platform.as_str())
        .bind(&campaign.external_id)
        .bind(&campaign.intent.name)
        .bind(campaign.intent.objective.as_str())
        .bind(campaign.intent.status.as_str())
        .bind(
            serde_json::to_value(&campaign.intent.budget)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&campaign.intent.schedule)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&campaign.intent.drift)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(campaign.revision)
        .bind(
            serde_json::to_value(&campaign.intent)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&campaign.remote_snapshot)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        campaign_id: Uuid,
    ) -> Result<Option<StoredCampaign>, AdvertisingError> {
        sqlx::query_as::<_, CampaignRow>(
            "SELECT id, tenant_id, connection_id, platform, external_id, revision, intent, remote_snapshot
             FROM advertising.campaigns WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
        ).bind(tenant_id).bind(campaign_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?
        .map(TryInto::try_into).transpose()
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        campaign: &StoredCampaign,
        expected_revision: i64,
    ) -> Result<bool, AdvertisingError> {
        let result = sqlx::query(
            "UPDATE advertising.campaigns SET external_id=$1, name=$2, objective=$3, status=$4,
               budget=$5, schedule=$6, drift=$7, revision=$8, intent=$9, remote_snapshot=$10,
               updated_at=now() WHERE tenant_id=$11 AND id=$12 AND revision=$13",
        )
        .bind(&campaign.external_id)
        .bind(&campaign.intent.name)
        .bind(campaign.intent.objective.as_str())
        .bind(campaign.intent.status.as_str())
        .bind(
            serde_json::to_value(&campaign.intent.budget)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&campaign.intent.schedule)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&campaign.intent.drift)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(campaign.revision)
        .bind(
            serde_json::to_value(&campaign.intent)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&campaign.remote_snapshot)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(campaign.tenant_id)
        .bind(campaign.id)
        .bind(expected_revision)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(result.rows_affected() == 1)
    }

    async fn list_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<StoredCampaign>, AdvertisingError> {
        let rows = sqlx::query_as::<_, CampaignRow>(
            "SELECT id, tenant_id, connection_id, platform, external_id, revision, intent, remote_snapshot
             FROM advertising.campaigns WHERE tenant_id = $1 ORDER BY created_at DESC, id DESC",
        )
        .bind(tenant_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn list_published_due_for_reconciliation(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        limit: i64,
    ) -> Result<Vec<StoredCampaign>, AdvertisingError> {
        let rows = sqlx::query_as::<_, CampaignRow>(
            "SELECT id, tenant_id, connection_id, platform, external_id, revision, intent, remote_snapshot
             FROM advertising.campaigns
             WHERE external_id IS NOT NULL
             ORDER BY updated_at ASC, id ASC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    async fn append_change(
        &self,
        scope: &mut ScopedTx<'_>,
        change: &CampaignChange,
    ) -> Result<(), AdvertisingError> {
        sqlx::query(
            "INSERT INTO advertising.campaign_changes
             (id, tenant_id, campaign_id, revision, mutation_id, actor_id, source, changed_fields,
              before_state, after_state, created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(change.id)
        .bind(change.tenant_id)
        .bind(change.campaign_id)
        .bind(change.revision)
        .bind(change.mutation_id)
        .bind(change.actor_id)
        .bind(change.source.as_str())
        .bind(
            serde_json::to_value(&change.changed_fields)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&change.before_state)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(
            serde_json::to_value(&change.after_state)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?,
        )
        .bind(change.created_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find_change_by_mutation(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        campaign_id: Uuid,
        mutation_id: Uuid,
    ) -> Result<Option<CampaignChange>, AdvertisingError> {
        sqlx::query_as::<_, CampaignChangeRow>(
            "SELECT id, tenant_id, campaign_id, revision, mutation_id, actor_id, source,
                    changed_fields, before_state, after_state, created_at
             FROM advertising.campaign_changes
             WHERE tenant_id=$1 AND campaign_id=$2 AND mutation_id=$3",
        )
        .bind(tenant_id)
        .bind(campaign_id)
        .bind(mutation_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?
        .map(TryInto::try_into)
        .transpose()
    }

    async fn list_changes(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        campaign_id: Uuid,
        limit: i64,
    ) -> Result<Vec<CampaignChange>, AdvertisingError> {
        sqlx::query_as::<_, CampaignChangeRow>(
            "SELECT id, tenant_id, campaign_id, revision, mutation_id, actor_id, source,
                    changed_fields, before_state, after_state, created_at
             FROM advertising.campaign_changes
             WHERE tenant_id=$1 AND campaign_id=$2
             ORDER BY created_at DESC, id DESC
             LIMIT $3",
        )
        .bind(tenant_id)
        .bind(campaign_id)
        .bind(limit)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
    }
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
