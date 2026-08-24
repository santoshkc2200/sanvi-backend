use std::{collections::BTreeMap, sync::Arc};

use crate::domain::{AdPlatformPort, AdvertisingError, PlatformKey};

/// Registered adapters are the single catalog source. Platform names do not
/// appear in HTTP handlers, validation, or application branching.
#[derive(Default)]
pub struct PlatformRegistry {
    adapters: BTreeMap<PlatformKey, Arc<dyn AdPlatformPort>>,
}

impl PlatformRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, adapter: Arc<dyn AdPlatformPort>) -> Result<(), AdvertisingError> {
        let key = adapter.descriptor().key;
        if self.adapters.insert(key.clone(), adapter).is_some() {
            return Err(AdvertisingError::InvalidInput(format!(
                "adapter already registered for {}",
                key.as_str()
            )));
        }
        Ok(())
    }

    pub fn adapters(&self) -> Vec<Arc<dyn AdPlatformPort>> {
        self.adapters.values().cloned().collect()
    }
}
