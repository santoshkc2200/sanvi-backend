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

    /// Resolve an adapter only at the integration boundary. Callers receive
    /// no platform-specific type, keeping the campaign workflow portable.
    pub fn adapter(&self, key: &PlatformKey) -> Result<Arc<dyn AdPlatformPort>, AdvertisingError> {
        self.adapters.get(key).cloned().ok_or_else(|| {
            AdvertisingError::PlatformUnavailable(format!(
                "no adapter registered for {}",
                key.as_str()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::FakeAdPlatform;

    #[test]
    fn resolves_registered_adapter_without_exposing_provider_type() {
        let registry = FakeAdPlatform::registered_catalog();
        let key = PlatformKey::parse("google_ads").unwrap();

        assert_eq!(registry.adapter(&key).unwrap().descriptor().key, key);
    }

    #[test]
    fn rejects_unregistered_adapter_at_integration_boundary() {
        let registry = PlatformRegistry::new();
        let key = PlatformKey::parse("unregistered").unwrap();

        assert!(matches!(
            registry.adapter(&key),
            Err(AdvertisingError::PlatformUnavailable(_))
        ));
    }
}
