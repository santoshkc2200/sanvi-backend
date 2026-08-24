use std::sync::Arc;

use crate::AdvertisingConfig;
use crate::application::ListPlatformCatalog;

/// Context state is assembled from registered adapters, never a handler list.
#[derive(Clone)]
pub struct AdvertisingState {
    pub config: AdvertisingConfig,
    pub catalog: Arc<ListPlatformCatalog>,
}
