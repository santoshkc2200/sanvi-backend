use crate::AdvertisingConfig;

/// Context state is deliberately configuration-only until TASK-010 supplies
/// the registry-derived platform catalog.
#[derive(Clone)]
pub struct AdvertisingState {
    pub config: AdvertisingConfig,
}
