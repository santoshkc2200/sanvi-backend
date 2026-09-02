use std::sync::Arc;

use crate::application::{
    AddAd, AddAdGroup, CreateCampaign, CreateConnection, CreateCreative, DeleteConnection,
    DeleteCreative, GetCampaign, GetCreative, GetCreativePreviews, HandleCallback,
    ListCampaignChanges, ListCampaigns, ListConnections, ListCreatives, ListPlatformCatalog,
    PatchCampaign, PauseCampaign, PublishCampaign, ReconcileCampaign, ResolveCampaignDrift,
    ResumeCampaign, StartAuthorization, UpdateAd, UpdateAdGroup, ValidateCampaign,
};
use crate::{AdvertisingConfig, OAuthStateManager};

/// Context state is assembled from registered adapters, never a handler list.
#[derive(Clone)]
pub struct AdvertisingState {
    pub config: AdvertisingConfig,
    /// Durable operation claim made before a provider mutation. This keeps a
    /// crash/retry chain on one client-generated key.
    pub idempotency: sanvi_persistence::IdempotencyStore,
    pub catalog: Arc<ListPlatformCatalog>,
    /// Worker and on-demand reconciliation share one use case that is unable
    /// to mutate a platform during observation.
    pub reconcile_campaign: Arc<ReconcileCampaign>,
    /// Explicit, idempotent drift choices. Observing drift never invokes it.
    pub resolve_campaign_drift: Arc<ResolveCampaignDrift>,
    /// Durable, signed OAuth CSRF state. No handler may mint or redeem state
    /// outside this boundary.
    pub oauth_state: OAuthStateManager,
    /// Campaign tree: draft/publish lifecycle, patch, pause/resume, and the
    /// attributed change log (TASK-012, 10.3).
    pub create_campaign: Arc<CreateCampaign>,
    pub get_campaign: Arc<GetCampaign>,
    pub list_campaigns: Arc<ListCampaigns>,
    pub validate_campaign: Arc<ValidateCampaign>,
    pub patch_campaign: Arc<PatchCampaign>,
    pub publish_campaign: Arc<PublishCampaign>,
    pub pause_campaign: Arc<PauseCampaign>,
    pub resume_campaign: Arc<ResumeCampaign>,
    pub list_campaign_changes: Arc<ListCampaignChanges>,
    pub add_ad_group: Arc<AddAdGroup>,
    pub update_ad_group: Arc<UpdateAdGroup>,
    pub add_ad: Arc<AddAd>,
    pub update_ad: Arc<UpdateAd>,
    /// Connections: OAuth start/callback, account selection, list, disconnect
    /// (TASK-011).
    pub start_authorization: Arc<StartAuthorization>,
    pub handle_callback: Arc<HandleCallback>,
    pub create_connection: Arc<CreateConnection>,
    pub list_connections: Arc<ListConnections>,
    pub delete_connection: Arc<DeleteConnection>,

    /// Creative management (TASK-013, 10.4).
    pub create_creative: Arc<CreateCreative>,
    pub get_creative: Arc<GetCreative>,
    pub list_creatives: Arc<ListCreatives>,
    pub delete_creative: Arc<DeleteCreative>,
    pub get_creative_previews: Arc<GetCreativePreviews>,

    /// Conversion capture & the directive gate (TASK-014, 10.5).
    pub tracking_ip_limiter: Arc<crate::adapters::rate_limit::QuotaLimiter>,
    pub tracking_domain_limiter: Arc<crate::adapters::rate_limit::QuotaLimiter>,
    pub capture: Arc<crate::application::tracking::capture::CaptureConversion>,
    pub settings_mgr: Arc<crate::application::tracking::settings::ManageTrackingSettings>,
    pub list_conversions: Arc<crate::application::tracking::conversions::ListConversions>,
    pub get_conversion_diagnostics:
        Arc<crate::application::tracking::conversions::GetConversionDiagnostics>,
    pub retry_conversion: Arc<crate::application::tracking::retry::RetryConversion>,

    /// Audience management and sync (TASK-015, 10.6).
    pub create_audience: Arc<crate::application::tracking::audiences::CreateAudience>,
    pub list_audiences: Arc<crate::application::tracking::audiences::ListAudiences>,
    pub refresh_audience: Arc<crate::application::tracking::audiences::RefreshAudience>,

    /// Metrics ingestion, rollups & the ROAS query layer (TASK-016, 10.7).
    /// Also backs `/ads/metrics/summary` and `/ads/metrics/export`, which
    /// share this one query use case rather than each getting their own.
    pub query_metrics: Arc<crate::application::QueryMetrics>,
    pub get_freshness: Arc<crate::application::GetFreshness>,

    /// Budget guardrails & spend alerts (TASK-017, 10.8).
    pub get_budget_caps: Arc<crate::application::GetBudgetCaps>,
    pub put_budget_cap: Arc<crate::application::PutBudgetCap>,
    pub get_campaign_budget_cap: Arc<crate::application::GetCampaignBudgetCap>,
    pub list_budget_alerts: Arc<crate::application::ListBudgetAlerts>,
    pub acknowledge_budget_alert: Arc<crate::application::AcknowledgeBudgetAlert>,
    pub get_spend_status: Arc<crate::application::GetSpendStatus>,
    /// Platform-admin health overview (TASK-018, 10.9).
    pub get_platform_ads_health: Arc<crate::application::GetPlatformAdsHealth>,
}
