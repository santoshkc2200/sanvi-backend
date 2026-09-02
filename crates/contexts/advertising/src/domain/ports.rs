use async_trait::async_trait;
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::hashing::HashedIdentifier;
use super::{
    AdvertisingConnection, Campaign, CapabilityMatrix, ConnectionHandle, ConversionEvent,
    HashedIds, PlatformKey, UploadReceipt, ValidationViolation,
};
use crate::domain::model::{
    AdGroupId, AdId, Audience, AudienceId, AudienceSyncReceipt, BudgetAlert, BudgetAlertId,
    BudgetCap, BudgetCapId, BudgetPeriod, CampaignChangeId, CampaignId, ConnectionId,
    ConsentSnapshot, ConversionId, CreativeId,
};
use sanvi_persistence::{PlatformScope, ScopedTx};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
    #[error("creative not found")]
    CreativeNotFound,
    #[error("conversion not found")]
    ConversionNotFound,
    #[error("audience not found")]
    AudienceNotFound,
    #[error("budget cap not found")]
    BudgetCapNotFound,
    #[error("budget alert not found")]
    BudgetAlertNotFound,
    #[error("new budget cap is below current period spend and requires confirmation")]
    CapBelowCurrentSpend,
    #[error("advertising token encryption is not configured")]
    EncryptionUnconfigured,
    #[error("advertising token encryption failure: {0}")]
    EncryptionFailure(String),
    #[error("advertising id generation failed: {0}")]
    IdGeneration(String),
}

impl From<sanvi_kernel::IdGenerationError> for AdvertisingError {
    fn from(err: sanvi_kernel::IdGenerationError) -> Self {
        Self::IdGeneration(err.to_string())
    }
}

impl From<sanvi_kernel::KernelError> for AdvertisingError {
    fn from(err: sanvi_kernel::KernelError) -> Self {
        Self::Storage(err.to_string())
    }
}

pub trait IdGenerator: Send + Sync {
    fn next_connection_id(&self) -> Result<ConnectionId, AdvertisingError>;
    fn next_campaign_id(&self) -> Result<CampaignId, AdvertisingError>;
    fn next_campaign_change_id(&self) -> Result<CampaignChangeId, AdvertisingError>;
    fn next_conversion_id(&self) -> Result<ConversionId, AdvertisingError>;
    fn next_creative_id(&self) -> Result<CreativeId, AdvertisingError>;
    fn next_ad_group_id(&self) -> Result<AdGroupId, AdvertisingError>;
    fn next_ad_id(&self) -> Result<AdId, AdvertisingError>;
    fn next_audience_id(&self) -> Result<AudienceId, AdvertisingError>;
    fn next_budget_cap_id(&self) -> Result<BudgetCapId, AdvertisingError>;
    fn next_budget_alert_id(&self) -> Result<BudgetAlertId, AdvertisingError>;
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
    /// Finalize a pending row. If its selected account already exists, retain
    /// that canonical row (and its campaign history), replace its token with
    /// the pending credential, and remove the temporary row.
    async fn finalize_pending(
        &self,
        scope: &mut ScopedTx<'_>,
        connection: &AdvertisingConnection,
    ) -> Result<AdvertisingConnection, AdvertisingError>;
    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<AdvertisingConnection>, AdvertisingError>;
    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        connection_id: ConnectionId,
    ) -> Result<Option<AdvertisingConnection>, AdvertisingError>;

    /// Cross-tenant, oldest-`last_synced_at`-first: the metrics ingestion
    /// sweep's feed (TASK-016). Staggers every connection through the
    /// scheduler so one tenant's connections cannot starve another's.
    /// Mirrors `CampaignRepository::list_published_due_for_reconciliation`'s
    /// `PlatformScope` pattern exactly.
    async fn list_due_for_metrics_ingestion(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: i64,
    ) -> Result<Vec<AdvertisingConnection>, AdvertisingError>;

    /// Cross-tenant, all connections across tenants for platform health reporting.
    async fn list_all_for_platform_scope(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<AdvertisingConnection>, AdvertisingError>;

    /// Cross-tenant, due for OAuth token refresh: active connections whose
    /// token is expiring or whose `last_synced_at` is stale.
    async fn list_due_for_token_refresh(
        &self,
        scope: &mut PlatformScope<'_>,
        stale_after_secs: i64,
        limit: i64,
    ) -> Result<Vec<AdvertisingConnection>, AdvertisingError>;
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
///
/// `spend` and `conversion_value` are always in the ad account's own currency
/// (TASK-016, slice 10.7) — the platform's own attribution of conversions to
/// this campaign, reported alongside (never blended with) Sanvi's own
/// order-based revenue view.
///
/// `conversions` is fractional: a platform's modeled/weighted attribution
/// (view-through, cross-device) can report a value like `1.5`, not just
/// whole units — an integer here would silently truncate real attribution
/// data before it ever reaches the rollup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    pub campaign_id: String,
    pub date: String,
    pub impressions: i64,
    pub clicks: i64,
    pub spend: super::Money,
    pub conversions: f64,
    pub conversion_value: super::Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedConversion {
    pub event_id: String,
}

/// Boundary for every network integration. No method reads ambient tenant state.
#[async_trait]
pub trait AdPlatformPort: Send + Sync {
    fn descriptor(&self) -> PlatformDescriptor;
    /// Every permission needed for campaign sync and conversion uploads.
    /// Health reports which of these were not granted at authorization time.
    fn required_scopes(&self) -> Vec<String>;
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

#[async_trait]
pub trait ConversionSinkPort: Send + Sync {
    fn descriptor(&self) -> PlatformDescriptor;
    async fn upload(
        &self,
        connection: &ConnectionHandle,
        event: &ConversionEvent,
    ) -> Result<UploadReceipt, AdvertisingError>;
    async fn retract(
        &self,
        connection: &ConnectionHandle,
        event: &ConversionEvent,
    ) -> Result<(), AdvertisingError>;
}

#[async_trait]
pub trait ConversionEventRepository: Send + Sync {
    /// Idempotent insert respecting the immutable `(tenant_id, dedupe_key)`
    /// identity owned by the conversion event.
    async fn insert_idempotent(
        &self,
        scope: &mut ScopedTx<'_>,
        event: &super::model::ConversionEvent,
    ) -> Result<(), AdvertisingError>;

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<super::model::ConversionEvent>, AdvertisingError>;

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: ConversionId,
    ) -> Result<Option<super::model::ConversionEvent>, AdvertisingError>;

    /// Events for one tenant whose `occurred_at` falls within `[from, to]`
    /// (inclusive) — the revenue join's per-window feed (TASK-016).
    async fn list_by_date_range(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        from: chrono::DateTime<chrono::Utc>,
        to: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<super::model::ConversionEvent>, AdvertisingError>;

    async fn update_upload_states(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: ConversionId,
        upload_states: BTreeMap<super::model::PlatformKey, super::model::UploadState>,
    ) -> Result<(), AdvertisingError>;

    /// Cross-tenant batch fetch: events across all tenants that have at least
    /// one platform pending upload, oldest `occurred_at` first, capped at `limit`.
    async fn list_pending_upload(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: i64,
    ) -> Result<Vec<super::model::ConversionEvent>, AdvertisingError>;

    /// Cross-tenant batch fetch: events across all tenants that have at least
    /// one platform in `uploaded` state, oldest `occurred_at` first, capped at `limit`.
    async fn list_uploaded(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: i64,
    ) -> Result<Vec<super::model::ConversionEvent>, AdvertisingError>;

    /// Cross-tenant count of conversion events that currently have at least one platform parked.
    async fn count_parked(&self, scope: &mut PlatformScope<'_>) -> Result<i64, AdvertisingError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentLookup {
    /// Payment-context adapter derives this from the actual payment record.
    /// Advertising never receives a raw payment id, so it cannot accidentally
    /// derive a checkout-based conversion id.
    pub conversion_event_id: String,
    pub value: super::model::Money,
}

#[async_trait]
pub trait PaymentLookupPort: Send + Sync {
    async fn lookup_payment_value(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        order_ref: &str,
    ) -> Result<Option<PaymentLookup>, AdvertisingError>;
}

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TrackingSettings {
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    /// event name -> (platform key -> the platform's conversion action)
    pub mappings: BTreeMap<String, BTreeMap<String, String>>,
}

#[async_trait]
pub trait TrackingSettingsRepository: Send + Sync {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Option<TrackingSettings>, AdvertisingError>;

    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        settings: &TrackingSettings,
    ) -> Result<(), AdvertisingError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VendorRole {
    ServiceProvider,
    ThirdParty,
}

#[async_trait]
pub trait VendorRolePort: Send + Sync {
    async fn lookup_role(
        &self,
        platform: &super::PlatformKey,
    ) -> Result<VendorRole, super::AdvertisingError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCreative {
    pub id: CreativeId,
    pub tenant_id: TenantId,
    pub connection_id: ConnectionId,
    pub platform: PlatformKey,
    pub external_id: Option<String>,
    pub creative: super::Creative,
    pub assets: Vec<super::AssetMetadata>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[async_trait]
pub trait CreativeRepository: Send + Sync {
    async fn insert(
        &self,
        scope: &mut ScopedTx<'_>,
        creative: &StoredCreative,
    ) -> Result<(), AdvertisingError>;

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        creative_id: CreativeId,
    ) -> Result<Option<StoredCreative>, AdvertisingError>;

    async fn list_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<StoredCreative>, AdvertisingError>;

    async fn delete(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        creative_id: CreativeId,
    ) -> Result<bool, AdvertisingError>;
}

#[async_trait]
pub trait AudienceSyncPort: Send + Sync {
    fn descriptor(&self) -> PlatformDescriptor;
    /// Full-state sync: `included` is the complete current membership after this
    /// call, `removed` is exactly what must be actively deleted/suppressed on the
    /// platform side (opted-out subjects previously included). A platform that
    /// cannot remove an already-uploaded member must still accept the call and
    /// report what it could not do via `AdvertisingError::Validation` with a
    /// `capability_unsupported` violation code (see `is_retraction_unsupported`
    /// in `src/application/tracking/retraction.rs` for the exact convention this
    /// crate already uses to detect that case — reuse or mirror it, don't invent
    /// a second convention).
    async fn sync_audience(
        &self,
        connection: &ConnectionHandle,
        external_ref: Option<&str>,
        included: &[HashedIdentifier],
        removed: &[HashedIdentifier],
    ) -> Result<AudienceSyncReceipt, AdvertisingError>;
}

#[async_trait]
pub trait AudienceRepository: Send + Sync {
    async fn insert(
        &self,
        scope: &mut PlatformScope<'_>,
        audience: &Audience,
    ) -> Result<(), AdvertisingError>;
    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        id: AudienceId,
    ) -> Result<Option<Audience>, AdvertisingError>;
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Audience>, AdvertisingError>;
    /// Cross-tenant batch fetch for the scheduled refresh sweep: audiences never
    /// synced, or last synced more than `stale_after_secs` ago. Mirror
    /// `ConversionEventRepository::list_uploaded`'s cross-tenant `PlatformScope`
    /// pattern exactly.
    async fn list_due_for_refresh(
        &self,
        scope: &mut PlatformScope<'_>,
        stale_after_secs: i64,
        limit: i64,
    ) -> Result<Vec<Audience>, AdvertisingError>;
    async fn update_after_sync(
        &self,
        scope: &mut PlatformScope<'_>,
        audience: &Audience,
    ) -> Result<(), AdvertisingError>;
}

/// A daily FX snapshot for display-only conversion (TASK-016, slice 10.7).
/// `rate` converts one unit of `base` into `quote`. Never persisted onto a
/// stored metric — the query layer applies it fresh on every read.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FxRate {
    pub rate: f64,
    pub date: chrono::NaiveDate,
}

/// Display-time currency conversion. Absence of a rate is a normal, expected
/// outcome (no snapshot for that pair/date yet) — callers render the
/// source-currency amount without a `rendered_*` view rather than fail.
#[async_trait]
pub trait FxRatePort: Send + Sync {
    async fn rate(
        &self,
        base: &str,
        quote: &str,
        date: chrono::NaiveDate,
    ) -> Result<Option<FxRate>, AdvertisingError>;
}

#[async_trait]
pub trait BudgetCapRepository: Send + Sync {
    async fn get_tenant_cap(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        period: BudgetPeriod,
    ) -> Result<Option<BudgetCap>, AdvertisingError>;

    async fn get_campaign_cap(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        campaign_id: CampaignId,
        period: BudgetPeriod,
    ) -> Result<Option<BudgetCap>, AdvertisingError>;

    async fn list_by_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<BudgetCap>, AdvertisingError>;

    async fn list_all_active_caps(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<BudgetCap>, AdvertisingError>;

    async fn upsert_cap(
        &self,
        scope: &mut ScopedTx<'_>,
        cap: &BudgetCap,
    ) -> Result<BudgetCap, AdvertisingError>;
}

#[async_trait]
pub trait BudgetAlertRepository: Send + Sync {
    async fn list_alerts(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        campaign_id: Option<CampaignId>,
        unacknowledged_only: bool,
        limit: i64,
    ) -> Result<Vec<BudgetAlert>, AdvertisingError>;

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: BudgetAlertId,
    ) -> Result<Option<BudgetAlert>, AdvertisingError>;

    async fn has_alert_fired(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        cap_id: BudgetCapId,
        period_start: chrono::NaiveDate,
        threshold: u32,
    ) -> Result<bool, AdvertisingError>;

    async fn record_alert(
        &self,
        scope: &mut ScopedTx<'_>,
        alert: &BudgetAlert,
    ) -> Result<BudgetAlert, AdvertisingError>;

    async fn acknowledge_alert(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: BudgetAlertId,
        user_id: sanvi_kernel::UserId,
        acknowledged_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<BudgetAlert>, AdvertisingError>;
}

#[async_trait]
pub trait BudgetNotifier: Send + Sync {
    async fn notify_threshold_80(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        spend: &super::Money,
        cap_amount: &super::Money,
        period: BudgetPeriod,
    ) -> Result<(), AdvertisingError>;

    async fn notify_cap_breached(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        spend: &super::Money,
        cap_amount: &super::Money,
        period: BudgetPeriod,
        action_taken: &str,
    ) -> Result<(), AdvertisingError>;

    async fn notify_provisional_warning(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        spend: &super::Money,
        cap_amount: &super::Money,
        lower_bound: &super::Money,
        period: BudgetPeriod,
    ) -> Result<(), AdvertisingError>;

    async fn notify_stale_warning(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        lag_hours: f64,
    ) -> Result<(), AdvertisingError>;

    async fn notify_spend_anomaly(
        &self,
        tenant_id: TenantId,
        scope_name: &str,
        current_spend: &super::Money,
        trailing_mean: &super::Money,
        sigma_multiple: f64,
    ) -> Result<(), AdvertisingError>;

    async fn notify_guardrails_rollback(&self, tenant_id: TenantId)
    -> Result<(), AdvertisingError>;
}
