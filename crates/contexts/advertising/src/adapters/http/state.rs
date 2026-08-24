use std::sync::Arc;

use crate::application::ListPlatformCatalog;
use crate::{AdvertisingConfig, OAuthStateManager};

/// Context state is assembled from registered adapters, never a handler list.
#[derive(Clone)]
pub struct AdvertisingState {
    pub config: AdvertisingConfig,
    pub catalog: Arc<ListPlatformCatalog>,
    /// Durable, signed OAuth CSRF state. No handler may mint or redeem state
    /// outside this boundary.
    pub oauth_state: OAuthStateManager,
}
