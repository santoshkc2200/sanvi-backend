//! The provider-catalog use case.
//!
//! Hexagonal by construction even in the skeleton slice: the use case
//! depends on a `ProviderCatalog` port, and 09.0's adapter is the empty
//! catalog. 09.1 replaces the adapter (real entries, per-tenant
//! availability) without touching the route, the contract, or the
//! generated client — which is the whole reason the port exists now.

use std::sync::Arc;

use async_trait::async_trait;

use super::ProvidersView;
use crate::domain::PaymentsError;

/// The port: which providers can this platform/tenant connect?
#[async_trait]
pub trait ProviderCatalog: Send + Sync {
    async fn list(&self, tenant_id: uuid::Uuid) -> Result<ProvidersView, PaymentsError>;
}

/// 09.0's adapter: the catalog exists, the entries do not yet. Returning
/// an empty (not error) view is deliberate — the pipeline is proven end
/// to end, and the settings page renders a real empty state.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyProviderCatalog;

#[async_trait]
impl ProviderCatalog for EmptyProviderCatalog {
    async fn list(&self, _tenant_id: uuid::Uuid) -> Result<ProvidersView, PaymentsError> {
        Ok(ProvidersView { providers: vec![] })
    }
}

/// `GET /api/v1/tenant/payments/providers`.
pub struct ListProviders {
    catalog: Arc<dyn ProviderCatalog>,
}

impl ListProviders {
    pub fn new(catalog: Arc<dyn ProviderCatalog>) -> Self {
        Self { catalog }
    }

    pub async fn execute(&self, tenant_id: uuid::Uuid) -> Result<ProvidersView, PaymentsError> {
        self.catalog.list(tenant_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_empty_catalog_answers_an_empty_envelope() {
        let use_case = ListProviders::new(Arc::new(EmptyProviderCatalog));
        let view = use_case.execute(uuid::Uuid::now_v7()).await.unwrap();
        assert!(view.providers.is_empty());
    }
}
