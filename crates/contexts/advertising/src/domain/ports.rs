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
    #[error("advertising token encryption is not configured")]
    EncryptionUnconfigured,
    #[error("advertising token encryption failure: {0}")]
    EncryptionFailure(String),
}

#[async_trait]
pub trait AdvertisingConnectionRepository: Send + Sync {
    /// A reconnect must retain its original id and campaign history. Merges
    /// on the natural key `(tenant_id, platform, external_account_id)` — not
    /// suitable for finalizing a pending connection, whose whole point is to
    /// *change* that key; use `update` there instead.
    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &AdvertisingConnection,
    ) -> Result<AdvertisingConnection, AdvertisingError>;
    /// Persist mutations to an already-known row, addressed by id: finalize
    /// (external_account_id changes, so it cannot go through `upsert`'s
    /// natural-key conflict target) and refresh success/failure.
    async fn update(
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

/// The callback's authorization code, ready to redeem. `redirect_uri` must
/// match what `begin_authorization` sent — the platform rejects a mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizationExchange {
    pub code: String,
    pub redirect_uri: String,
}

/// The result of redeeming an authorization code: a refresh token (never
/// logged, never returned to a caller outside the connections service) and
/// the accounts it can see, for the picker. The exchange endpoint of every
/// platform this port supports returns both in one round trip, so this is
/// one port method rather than two.
#[derive(Clone, Serialize, Deserialize)]
pub struct ExchangedAuthorization {
    pub refresh_token: String,
    pub granted_scopes: Vec<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub accounts: Vec<PlatformAccount>,
}

impl std::fmt::Debug for ExchangedAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangedAuthorization")
            .field("refresh_token", &"[REDACTED]")
            .field("granted_scopes", &self.granted_scopes)
            .field("expires_at", &self.expires_at)
            .field("accounts", &self.accounts)
            .finish()
    }
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
    /// Redeem a callback's authorization code for a refresh token and the
    /// accounts it can see. The code is single-use: call this exactly once
    /// per callback, before anything is persisted.
    async fn exchange_authorization(
        &self,
        exchange: &AuthorizationExchange,
    ) -> Result<ExchangedAuthorization, AdvertisingError>;
    async fn create_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign: Campaign,
        mutation_id: Uuid,
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
