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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}
