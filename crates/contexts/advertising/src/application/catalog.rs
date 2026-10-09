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

    #[tracing::instrument(name = "advertising.list_platform_catalog", skip_all)]
    pub fn execute(&self) -> Vec<PlatformDescriptor> {
        self.registry
            .adapters()
            .into_iter()
            .map(|adapter| adapter.descriptor())
            .collect()
    }

    /// Catalog entries paired with the adapter's OAuth scopes. The HTTP
    /// `PlatformView` reads this so the pre-connect screen enumerates
    /// requested permissions from data (frontend TASK-011) instead of
    /// hard-coding per-platform scope lists.
    pub fn execute_with_scopes(&self) -> Vec<(PlatformDescriptor, Vec<String>)> {
        self.registry
            .adapters()
            .into_iter()
            .map(|adapter| {
                let scopes = adapter.required_scopes();
                (adapter.descriptor(), scopes)
            })
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
