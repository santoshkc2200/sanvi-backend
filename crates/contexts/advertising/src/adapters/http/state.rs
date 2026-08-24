use std::sync::Arc;

use crate::application::{
    AddAd, AddAdGroup, CreateCampaign, GetCampaign, ListCampaignChanges, ListCampaigns,
    ListPlatformCatalog, PatchCampaign, PauseCampaign, PublishCampaign, ReconcileCampaign,
    ResolveCampaignDrift, ResumeCampaign, UpdateAd, UpdateAdGroup, ValidateCampaign,
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
}
