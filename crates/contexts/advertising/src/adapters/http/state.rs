use std::sync::Arc;

use crate::application::{ListPlatformCatalog, ReconcileCampaign};
use crate::{AdvertisingConfig, OAuthStateManager};

/// Context state is assembled from registered adapters, never a handler list.
#[derive(Clone)]
pub struct AdvertisingState {
    pub config: AdvertisingConfig,
    pub catalog: Arc<ListPlatformCatalog>,
    /// Worker and on-demand reconciliation share one use case that is unable
    /// to mutate a platform during observation.
    pub reconcile_campaign: Arc<ReconcileCampaign>,
    /// Durable, signed OAuth CSRF state. No handler may mint or redeem state
    /// outside this boundary.
    pub oauth_state: OAuthStateManager,
}
