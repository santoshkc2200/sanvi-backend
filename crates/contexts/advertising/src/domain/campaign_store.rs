use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{AdvertisingError, Campaign, PlatformKey};
use sanvi_persistence::ScopedTx;

/// Persisted campaign intent plus the last version observed at the platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCampaign {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub connection_id: Uuid,
    pub platform: PlatformKey,
    pub external_id: Option<String>,
    pub revision: i64,
    pub intent: Campaign,
    pub remote_snapshot: Campaign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeSource {
    Sanvi,
    Platform,
}

impl ChangeSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sanvi => "sanvi",
            Self::Platform => "platform",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CampaignChange {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub campaign_id: Uuid,
    pub revision: i64,
    pub mutation_id: Option<Uuid>,
    pub actor_id: Option<Uuid>,
    pub source: ChangeSource,
    pub changed_fields: Vec<String>,
    pub before_state: Campaign,
    pub after_state: Campaign,
    pub created_at: DateTime<Utc>,
}

#[async_trait]
pub trait CampaignRepository: Send + Sync {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        campaign: &StoredCampaign,
    ) -> Result<(), AdvertisingError>;

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        campaign_id: Uuid,
    ) -> Result<Option<StoredCampaign>, AdvertisingError>;

    /// Newest-first. Callers needing a specific connection or platform filter
    /// client-side; the list is tenant-bounded and expected to stay small
    /// enough for one page until phase 10.7 adds pagination alongside metrics.
    async fn list_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<StoredCampaign>, AdvertisingError>;

    /// Cross-tenant, oldest-checked-first: the reconciliation sweep's feed.
    /// A published campaign (`external_id` set) is due regardless of which
    /// tenant last touched it, so one tenant's edits cannot starve every
    /// other tenant's drift detection. Takes the audited platform scope,
    /// never a tenant-bound transaction — see [`sanvi_persistence::TxManager::begin_unscoped`].
    async fn list_published_due_for_reconciliation(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        limit: i64,
    ) -> Result<Vec<StoredCampaign>, AdvertisingError>;

    /// Record a completed pull without changing campaign intent or revision.
    /// This is the sweep cursor for unchanged remote state.
    async fn mark_reconciled(
        &self,
        scope: &mut sanvi_persistence::PlatformScope<'_>,
        campaign_id: Uuid,
    ) -> Result<(), AdvertisingError>;

    /// Compare-and-swap update. A zero-row update is a stale `If-Match`
    /// revision; callers must load current state before returning `409`.
    async fn update(
        &self,
        scope: &mut ScopedTx<'_>,
        campaign: &StoredCampaign,
        expected_revision: i64,
    ) -> Result<bool, AdvertisingError>;

    async fn append_change(
        &self,
        scope: &mut ScopedTx<'_>,
        change: &CampaignChange,
    ) -> Result<(), AdvertisingError>;

    async fn find_change_by_mutation(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        campaign_id: Uuid,
        mutation_id: Uuid,
    ) -> Result<Option<CampaignChange>, AdvertisingError>;

    /// Newest-first audit trail for one campaign. This is what answers a
    /// spend dispute, so it is append-only and never filtered by source.
    async fn list_changes(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        campaign_id: Uuid,
        limit: i64,
    ) -> Result<Vec<CampaignChange>, AdvertisingError>;
}
