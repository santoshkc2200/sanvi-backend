use std::sync::Arc;

use crate::domain::PlatformDescriptor;

use super::PlatformRegistry;

/// Registry-derived catalog use case. Entitlement checks remain at the HTTP
/// boundary so the endpoint can show an upgrade state for each entry.
pub struct ListPlatformCatalog {
    registry: Arc<PlatformRegistry>,
}

impl ListPlatformCatalog {
    pub fn new(registry: Arc<PlatformRegistry>) -> Self {
        Self { registry }
    }

    pub fn execute(&self) -> Vec<PlatformDescriptor> {
        self.registry
            .adapters()
            .into_iter()
            .map(|adapter| adapter.descriptor())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::adapters::FakeAdPlatform;

    use super::ListPlatformCatalog;

    #[test]
    fn catalog_is_derived_from_registered_adapters() {
        let catalog = ListPlatformCatalog::new(Arc::new(FakeAdPlatform::registered_catalog()));
        let entries = catalog.execute();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| !entry.entitlement_key.is_empty())
        );
    }
}
