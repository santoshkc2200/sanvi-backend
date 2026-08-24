use std::sync::Arc;

use crate::application::{ListPlatformCatalog, ReconcileCampaign, ResolveCampaignDrift};
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
}
