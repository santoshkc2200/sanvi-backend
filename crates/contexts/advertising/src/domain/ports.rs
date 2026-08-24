use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    AdvertisingConnection, Campaign, CapabilityMatrix, ConnectionHandle, HashedIds, PlatformKey,
    ValidationViolation,
};
use crate::domain::model::ConsentSnapshot;
use sanvi_persistence::ScopedTx;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum AdvertisingError {
    #[error("invalid advertising input: {0}")]
    InvalidInput(String),
    #[error("platform unavailable: {0}")]
    PlatformUnavailable(String),
    #[error("advertising storage failure: {0}")]
    Storage(String),
    #[error("campaign revision is stale")]
    StaleRevision,
    #[error("campaign not found")]
    CampaignNotFound,
    #[error("campaign violates platform capabilities")]
    Validation(Vec<ValidationViolation>),
    #[error("connection is not active")]
    ConnectionNotActive,
    #[error("advertising connection not found")]
    ConnectionNotFound,
    #[error("campaign is not yet published")]
    CampaignNotPublished,
    #[error("ad group not found")]
    AdGroupNotFound,
    #[error("ad not found")]
    AdNotFound,
}

#[async_trait]
pub trait AdvertisingConnectionRepository: Send + Sync {
    /// A reconnect must retain its original id and campaign history.
    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &AdvertisingConnection,
    ) -> Result<AdvertisingConnection, AdvertisingError>;
    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<AdvertisingConnection>, AdvertisingError>;
    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        connection_id: Uuid,
    ) -> Result<Option<AdvertisingConnection>, AdvertisingError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformDescriptor {
    pub key: PlatformKey,
    pub display_name: String,
    pub entitlement_key: String,
    pub capability_matrix: CapabilityMatrix,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformAccount {
    pub external_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationRequest {
    pub redirect_uri: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationStart {
    pub authorization_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricQuery {
    pub from: String,
    pub to: String,
}

/// A provider-normalized metrics row. Later reporting work may add dimensions without
/// returning a request-shaped value from this port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricPoint {
    pub campaign_id: String,
    pub date: String,
    pub impressions: i64,
    pub clicks: i64,
    pub spend: super::Money,
    pub conversions: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedConversion {
    pub event_id: String,
}

/// Boundary for every network integration. No method reads ambient tenant state.
#[async_trait]
pub trait AdPlatformPort: Send + Sync {
    fn descriptor(&self) -> PlatformDescriptor;
    async fn begin_authorization(
        &self,
        connection: &ConnectionHandle,
        request: &AuthorizationRequest,
    ) -> Result<AuthorizationStart, AdvertisingError>;
    async fn list_accounts(
        &self,
        connection: &ConnectionHandle,
    ) -> Result<Vec<PlatformAccount>, AdvertisingError>;
    async fn create_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign: Campaign,
    ) -> Result<Campaign, AdvertisingError>;
    async fn get_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Option<Campaign>, AdvertisingError>;
    async fn update_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign: Campaign,
        mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError>;
    async fn pause_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError>;
    async fn resume_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError>;
    async fn delete_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<(), AdvertisingError>;
    async fn fetch_metrics(
        &self,
        connection: &ConnectionHandle,
        request: &MetricQuery,
    ) -> Result<Vec<MetricPoint>, AdvertisingError>;
    async fn upload(
        &self,
        connection: &ConnectionHandle,
        conversion: UploadedConversion,
        ids: HashedIds,
        consent: ConsentSnapshot,
    ) -> Result<(), AdvertisingError>;
}
