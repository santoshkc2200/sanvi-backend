//! SQLx persistence for tenant advertising connections.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::ScopedTx;
use uuid::Uuid;

use crate::domain::{
    AdvertisingConnection, AdvertisingConnectionRepository, AdvertisingError, CampaignChange,
    CampaignRepository, ChangeSource, ConnectionStatus, CreativeRepository, PlatformAccount,
    PlatformKey, SecretRef, StoredCampaign, StoredCreative, SyncError,
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
    pending_accounts: Option<serde_json::Value>,
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
        let pending_accounts = row
            .pending_accounts
            .map(serde_json::from_value::<Vec<PlatformAccount>>)
            .transpose()
            .map_err(|error| {
                AdvertisingError::Storage(format!("invalid advertising pending accounts: {error}"))
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
            pending_accounts,
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

fn pending_accounts_json(
    accounts: Option<&[PlatformAccount]>,
) -> Result<Option<serde_json::Value>, AdvertisingError> {
    accounts
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| {
            AdvertisingError::Storage(format!(
                "advertising pending accounts serialization failed: {error}"
            ))
        })
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

    async fn mark_reconciled(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        campaign_id: Uuid,
    ) -> Result<(), AdvertisingError> {
        sqlx::query("UPDATE advertising.campaigns SET updated_at=now() WHERE id=$1")
            .bind(campaign_id)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?;
        Ok(())
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
              last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at,
              pending_accounts)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
             ON CONFLICT (tenant_id, platform, external_account_id) DO UPDATE SET
               account_name = EXCLUDED.account_name, currency = EXCLUDED.currency,
               timezone = EXCLUDED.timezone, status = EXCLUDED.status, scopes = EXCLUDED.scopes,
               token_ref = EXCLUDED.token_ref, connected_by = EXCLUDED.connected_by,
               connected_at = EXCLUDED.connected_at, token_expires_at = EXCLUDED.token_expires_at,
               last_synced_at = EXCLUDED.last_synced_at, last_error = EXCLUDED.last_error,
               disconnected_notified_at = EXCLUDED.disconnected_notified_at,
               dependent_jobs_paused_at = EXCLUDED.dependent_jobs_paused_at,
               pending_accounts = EXCLUDED.pending_accounts,
               updated_at = now()
             RETURNING id, tenant_id, platform, external_account_id, account_name, currency,
               timezone, status, scopes, token_ref, connected_by, connected_at, token_expires_at,
               last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at,
               pending_accounts",
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
        .bind(pending_accounts_json(connection.pending_accounts())?)
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        row.try_into()
    }

    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &AdvertisingConnection,
    ) -> Result<AdvertisingConnection, AdvertisingError> {
        let row = sqlx::query_as::<_, ConnectionRow>(
            "UPDATE advertising.connections SET
               external_account_id = $3, account_name = $4, currency = $5, timezone = $6,
               status = $7, scopes = $8, token_ref = $9, token_expires_at = $10,
               last_synced_at = $11, last_error = $12, disconnected_notified_at = $13,
               dependent_jobs_paused_at = $14, pending_accounts = $15, updated_at = now()
             WHERE tenant_id = $1 AND id = $2
             RETURNING id, tenant_id, platform, external_account_id, account_name, currency,
               timezone, status, scopes, token_ref, connected_by, connected_at, token_expires_at,
               last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at,
               pending_accounts",
        )
        .bind(connection.tenant_id())
        .bind(connection.id())
        .bind(connection.external_account_id())
        .bind(connection.account_name())
        .bind(connection.currency())
        .bind(connection.timezone())
        .bind(connection.status().as_str())
        .bind(connection.granted_scopes())
        .bind(connection.token_ref().persisted_locator())
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
        .bind(pending_accounts_json(connection.pending_accounts())?)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?
        .ok_or(AdvertisingError::ConnectionNotFound)?;
        row.try_into()
    }

    async fn finalize_pending(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &AdvertisingConnection,
    ) -> Result<AdvertisingConnection, AdvertisingError> {
        let pending_id = connection.id();
        let stored = self.upsert(scope, connection).await?;
        if stored.id() == pending_id {
            return Ok(stored);
        }

        let moved = sqlx::query(
            "INSERT INTO advertising.connection_tokens (connection_id, tenant_id, ciphertext)
             SELECT $3, tenant_id, ciphertext
             FROM advertising.connection_tokens
             WHERE tenant_id = $1 AND connection_id = $2
             ON CONFLICT (connection_id) DO UPDATE SET
               ciphertext = EXCLUDED.ciphertext, tenant_id = EXCLUDED.tenant_id, updated_at = now()",
        )
        .bind(connection.tenant_id())
        .bind(pending_id)
        .bind(stored.id())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        if moved.rows_affected() != 1 {
            return Err(AdvertisingError::Storage(
                "advertising pending connection token missing".to_string(),
            ));
        }

        sqlx::query(
            "UPDATE advertising.connections SET token_ref = $3, updated_at = now()
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(connection.tenant_id())
        .bind(stored.id())
        .bind(stored.id().to_string())
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        sqlx::query("DELETE FROM advertising.connections WHERE tenant_id = $1 AND id = $2")
            .bind(connection.tenant_id())
            .bind(pending_id)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(storage)?;
        self.find_by_id(scope, connection.tenant_id(), stored.id())
            .await?
            .ok_or(AdvertisingError::ConnectionNotFound)
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<AdvertisingConnection>, AdvertisingError> {
        let rows = sqlx::query_as::<_, ConnectionRow>(
            "SELECT id, tenant_id, platform, external_account_id, account_name, currency,
               timezone, status, scopes, token_ref, connected_by, connected_at, token_expires_at,
               last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at,
               pending_accounts
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
               last_synced_at, last_error, disconnected_notified_at, dependent_jobs_paused_at,
               pending_accounts
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

use crate::domain::{
    ConversionEvent, ConversionEventRepository, TrackingSettings, TrackingSettingsRepository,
};

#[derive(sqlx::FromRow)]
struct ConversionEventRow {
    id: Uuid,
    tenant_id: Uuid,
    event_id: String,
    name: String,
    occurred_at: chrono::DateTime<chrono::Utc>,
    value_minor: Option<i64>,
    currency: Option<String>,
    value_source: Option<String>,
    order_ref: Option<String>,
    click_ids: serde_json::Value,
    hashed_ids: serde_json::Value,
    consent: serde_json::Value,
    upload_state: serde_json::Value,
}

fn row_to_conversion_event(row: ConversionEventRow) -> Result<ConversionEvent, AdvertisingError> {
    let map_json_err = |err: serde_json::Error| AdvertisingError::Storage(err.to_string());
    Ok(ConversionEvent {
        id: row.id,
        tenant_id: row.tenant_id,
        event_id: row.event_id,
        name: row.name,
        occurred_at: row.occurred_at,
        value: match (row.value_minor, row.currency) {
            (Some(amount_minor), Some(currency)) => {
                Some(crate::domain::Money::new(amount_minor, &currency)?)
            }
            _ => None,
        },
        // Never fall back to `ClientReported`: that would relabel a stored
        // payment-record value as an untrusted client hint the moment a
        // migration adds a variant this build doesn't know.
        value_source: row
            .value_source
            .map(|source| match source.as_str() {
                "payment_record" => Ok(crate::domain::ValueSource::PaymentRecord),
                "client_reported" => Ok(crate::domain::ValueSource::ClientReported),
                unknown => Err(AdvertisingError::Storage(format!(
                    "unknown conversion value source {unknown:?}"
                ))),
            })
            .transpose()?,
        order_ref: row.order_ref,
        click_ids: serde_json::from_value(row.click_ids).map_err(map_json_err)?,
        hashed_identifiers: serde_json::from_value(row.hashed_ids).map_err(map_json_err)?,
        consent: serde_json::from_value(row.consent).map_err(map_json_err)?,
        upload_state: serde_json::from_value(row.upload_state).map_err(map_json_err)?,
    })
}

#[async_trait]
impl ConversionEventRepository for SqlxCampaignRepository {
    async fn insert_idempotent(
        &self,
        scope: &mut ScopedTx<'_>,
        event: &ConversionEvent,
    ) -> Result<(), AdvertisingError> {
        let map_err = |e: sqlx::Error| AdvertisingError::Storage(e.to_string());

        // The dedup row owns the (tenant_id, event_id) unique key that
        // `conversion_events` foreign-keys against; a retried or duplicated
        // beacon loses the race here and never reaches the insert below —
        // that's the whole idempotency guarantee, in one INSERT.
        let dedup_result = sqlx::query(
            "INSERT INTO advertising.conversion_event_dedup (tenant_id, event_id, occurred_at)
             VALUES ($1, $2, $3) ON CONFLICT (tenant_id, event_id) DO NOTHING",
        )
        .bind(event.tenant_id)
        .bind(&event.event_id)
        .bind(event.occurred_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_err)?;

        if dedup_result.rows_affected() == 0 {
            return Ok(());
        }

        let value_minor = event.value.as_ref().map(|money| money.amount_minor);
        let currency = event.value.as_ref().map(|money| money.currency.clone());
        let value_source = event.value_source.as_ref().map(|source| match source {
            crate::domain::ValueSource::PaymentRecord => "payment_record",
            crate::domain::ValueSource::ClientReported => "client_reported",
        });
        let click_ids = serde_json::to_value(&event.click_ids)
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let hashed_ids = serde_json::to_value(&event.hashed_identifiers)
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let consent = serde_json::to_value(&event.consent)
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        let upload_state = serde_json::to_value(&event.upload_state)
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        sqlx::query(
            "INSERT INTO advertising.conversion_events
               (id, tenant_id, event_id, name, occurred_at, value_minor, currency,
                value_source, order_ref, click_ids, hashed_ids, consent, upload_state)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(event.id)
        .bind(event.tenant_id)
        .bind(&event.event_id)
        .bind(&event.name)
        .bind(event.occurred_at)
        .bind(value_minor)
        .bind(currency)
        .bind(value_source)
        .bind(&event.order_ref)
        .bind(click_ids)
        .bind(hashed_ids)
        .bind(consent)
        .bind(upload_state)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(map_err)?;

        Ok(())
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<ConversionEvent>, AdvertisingError> {
        let rows = sqlx::query_as::<_, ConversionEventRow>(
            "SELECT id, tenant_id, event_id, name, occurred_at, value_minor, currency,
                    value_source, order_ref, click_ids, hashed_ids, consent, upload_state
             FROM advertising.conversion_events
             WHERE tenant_id = $1
             ORDER BY occurred_at DESC
             LIMIT 200",
        )
        .bind(tenant_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        rows.into_iter().map(row_to_conversion_event).collect()
    }
}

#[derive(Default)]
pub struct SqlxTrackingSettingsRepository;

impl SqlxTrackingSettingsRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl TrackingSettingsRepository for SqlxTrackingSettingsRepository {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Option<TrackingSettings>, AdvertisingError> {
        use sqlx::Row;
        let row =
            sqlx::query("SELECT mappings FROM advertising.tracking_settings WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_optional(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;

        if let Some(r) = row {
            let val: serde_json::Value = r.get("mappings");
            let mappings: std::collections::BTreeMap<
                String,
                std::collections::BTreeMap<String, String>,
            > = serde_json::from_value(val)
                .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
            Ok(Some(TrackingSettings {
                tenant_id,
                mappings,
            }))
        } else {
            Ok(None)
        }
    }

    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        settings: &TrackingSettings,
    ) -> Result<(), AdvertisingError> {
        let json = serde_json::to_value(&settings.mappings)
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        sqlx::query("INSERT INTO advertising.tracking_settings (tenant_id, mappings) VALUES ($1, $2) ON CONFLICT (tenant_id) DO UPDATE SET mappings = $2, updated_at = now()")
            .bind(settings.tenant_id)
            .bind(json)
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct CreativeRow {
    id: Uuid,
    tenant_id: Uuid,
    external_id: Option<String>,
    specification: serde_json::Value,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CreativeSpecificationPayload {
    connection_id: Uuid,
    platform: PlatformKey,
    creative: crate::domain::Creative,
    #[serde(default)]
    assets: Vec<crate::domain::AssetMetadata>,
}

impl TryFrom<CreativeRow> for StoredCreative {
    type Error = AdvertisingError;

    fn try_from(row: CreativeRow) -> Result<Self, Self::Error> {
        let payload: CreativeSpecificationPayload = serde_json::from_value(row.specification)
            .map_err(|error| {
                AdvertisingError::Storage(format!("invalid creative specification: {error}"))
            })?;
        Ok(Self {
            id: row.id,
            tenant_id: row.tenant_id,
            connection_id: payload.connection_id,
            platform: payload.platform,
            external_id: row.external_id,
            creative: payload.creative,
            assets: payload.assets,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxCreativeRepository;

impl SqlxCreativeRepository {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl CreativeRepository for SqlxCreativeRepository {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        creative: &StoredCreative,
    ) -> Result<(), AdvertisingError> {
        let payload = CreativeSpecificationPayload {
            connection_id: creative.connection_id,
            platform: creative.platform.clone(),
            creative: creative.creative.clone(),
            assets: creative.assets.clone(),
        };
        let specification =
            serde_json::to_value(&payload).map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        sqlx::query(
            "INSERT INTO advertising.creatives (id, tenant_id, external_id, specification, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(creative.id)
        .bind(creative.tenant_id)
        .bind(&creative.external_id)
        .bind(specification)
        .bind(creative.created_at)
        .bind(creative.updated_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        creative_id: Uuid,
    ) -> Result<Option<StoredCreative>, AdvertisingError> {
        let row = sqlx::query_as::<_, CreativeRow>(
            "SELECT id, tenant_id, external_id, specification, created_at, updated_at
             FROM advertising.creatives
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(creative_id)
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        row.map(StoredCreative::try_from).transpose()
    }

    async fn list_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<StoredCreative>, AdvertisingError> {
        let rows = sqlx::query_as::<_, CreativeRow>(
            "SELECT id, tenant_id, external_id, specification, created_at, updated_at
             FROM advertising.creatives
             WHERE tenant_id = $1
             ORDER BY created_at DESC",
        )
        .bind(tenant_id)
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        rows.into_iter().map(StoredCreative::try_from).collect()
    }

    async fn delete(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        creative_id: Uuid,
    ) -> Result<bool, AdvertisingError> {
        let result = sqlx::query(
            "DELETE FROM advertising.creatives
             WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant_id)
        .bind(creative_id)
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;

        Ok(result.rows_affected() > 0)
    }
}
