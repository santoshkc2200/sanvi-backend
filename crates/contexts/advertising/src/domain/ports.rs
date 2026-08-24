use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{Campaign, CapabilityMatrix, ConnectionHandle, HashedIds, PlatformKey};
use crate::domain::model::ConsentSnapshot;

#[derive(Debug, thiserror::Error)]
pub enum AdvertisingError {
    #[error("invalid advertising input: {0}")]
    InvalidInput(String),
    #[error("platform unavailable: {0}")]
    PlatformUnavailable(String),
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
