//! The provider-catalog use case.
//!
//! Hexagonal by construction: the use case depends on a `ProviderCatalog`
//! port. 09.1's adapter is the registry-backed catalog — entries are
//! *derived* from the registered adapters (`ProviderDescriptor`), never
//! hardcoded here or in the handler. A provider appears in the settings
//! page because the composition root registered it; that is the entire
//! mechanism, and the acceptance test proves it by registering one and
//! watching the response change with zero code edits.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::TenantId;

use super::ProvidersView;
use crate::domain::PaymentsError;
use crate::domain::provider::CountryCode;

/// The port: which providers can this platform/tenant connect?
#[async_trait]
pub trait ProviderCatalog: Send + Sync {
    async fn list(
        &self,
        tenant_id: TenantId,
        tenant_country: Option<CountryCode>,
    ) -> Result<ProvidersView, PaymentsError>;
}

/// 09.0's degenerate adapter: no providers registered, nothing listed.
/// Kept because it is the honest zero state of a registry-less wiring
/// (and the unit test's control group); a `RegistryProviderCatalog` over
/// an empty registry behaves identically.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyProviderCatalog;

#[async_trait]
impl ProviderCatalog for EmptyProviderCatalog {
    async fn list(
        &self,
        _tenant_id: TenantId,
        _tenant_country: Option<CountryCode>,
    ) -> Result<ProvidersView, PaymentsError> {
        Ok(ProvidersView { providers: vec![] })
    }
}

/// The registry-backed catalog. Two independent rules keep test-only
/// providers out of production even if wiring is wrong in one place:
///
/// 1. the composition root registers non-production adapters only in
///    non-production environments;
/// 2. **this** adapter refuses to *list* any descriptor with
///    `production: false` when the environment is production.
///
/// Per-tenant `available` is country support: a provider that cannot
/// onboard the tenant's country renders its unavailable state, not a dead
/// connect button. An unknown tenant country stays `available: true` —
/// missing data must not lock tenants out of the catalog.
pub struct RegistryProviderCatalog {
    registry: Arc<super::registry::ProviderRegistry>,
    environment: super::registry::RuntimeEnvironment,
}

impl RegistryProviderCatalog {
    pub fn new(
        registry: Arc<super::registry::ProviderRegistry>,
        environment: super::registry::RuntimeEnvironment,
    ) -> Self {
        Self {
            registry,
            environment,
        }
    }
}

#[async_trait]
impl ProviderCatalog for RegistryProviderCatalog {
    async fn list(
        &self,
        _tenant_id: TenantId,
        tenant_country: Option<CountryCode>,
    ) -> Result<ProvidersView, PaymentsError> {
        let mut providers = Vec::new();
        for adapter in self.registry.adapters() {
            let descriptor = adapter.descriptor();
            if !descriptor.production
                && self.environment == super::registry::RuntimeEnvironment::Production
            {
                // Gate 2 — see the type docs. Never listed in production,
                // even if someone registered it.
                continue;
            }
            let available = tenant_country
                .as_ref()
                .map(|country| descriptor.supported_countries.contains(country))
                .unwrap_or(true);
            providers.push(super::ProviderView {
                kind: descriptor.kind.as_str().to_string(),
                available,
                display_name: descriptor.display_name.clone(),
                supported_countries: descriptor
                    .supported_countries
                    .iter()
                    .map(|c| c.as_str().to_string())
                    .collect(),
                requires_onboarding: descriptor.requires_onboarding,
            });
        }
        Ok(ProvidersView { providers })
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

    #[tracing::instrument(name = "payments.list_providers", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        tenant_country: Option<CountryCode>,
    ) -> Result<ProvidersView, PaymentsError> {
        self.catalog.list(tenant_id, tenant_country).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::fake_provider::FakeProvider;
    use crate::application::registry::{ProviderRegistry, RuntimeEnvironment};
    use crate::domain::provider::ProviderKind;

    #[tokio::test]
    async fn the_empty_catalog_answers_an_empty_envelope() {
        let use_case = ListProviders::new(Arc::new(EmptyProviderCatalog));
        let view = use_case
            .execute(TenantId::try_from(1_000_001_i64).unwrap(), None)
            .await
            .unwrap();
        assert!(view.providers.is_empty());
    }

    #[tokio::test]
    async fn registering_the_fake_provider_makes_it_appear_in_the_catalog() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(FakeProvider::new())).unwrap();
        let catalog =
            RegistryProviderCatalog::new(Arc::new(registry), RuntimeEnvironment::NonProduction);

        let view = catalog
            .list(TenantId::try_from(1_000_001_i64).unwrap(), None)
            .await
            .unwrap();
        assert_eq!(view.providers.len(), 1);
        let entry = &view.providers[0];
        assert_eq!(entry.kind, "fake");
        assert_eq!(entry.display_name, "Test provider");
        assert!(
            entry.available,
            "unknown tenant country must not lock anyone out"
        );
        assert!(entry.requires_onboarding);
        assert_eq!(entry.supported_countries, ["US", "JP"]);
    }

    #[tokio::test]
    async fn a_production_environment_contains_no_fake_entry() {
        // Even when the adapter *is* registered (wiring mistake), the
        // catalog refuses to list it — gate 2.
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(FakeProvider::new())).unwrap();
        let catalog =
            RegistryProviderCatalog::new(Arc::new(registry), RuntimeEnvironment::Production);

        let view = catalog
            .list(TenantId::try_from(1_000_001_i64).unwrap(), None)
            .await
            .unwrap();
        assert!(
            view.providers.is_empty(),
            "production must list no fake entry"
        );
    }

    #[tokio::test]
    async fn availability_follows_the_tenants_country() {
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(FakeProvider::new())).unwrap();
        let catalog =
            RegistryProviderCatalog::new(Arc::new(registry), RuntimeEnvironment::NonProduction);

        // The fake supports US/JP: a JP tenant connects, a GB tenant sees
        // the unavailable state.
        let jp = catalog
            .list(
                TenantId::try_from(1_000_001_i64).unwrap(),
                Some(CountryCode::parse("JP").unwrap()),
            )
            .await
            .unwrap();
        assert!(jp.providers[0].available);

        let gb = catalog
            .list(
                TenantId::try_from(1_000_001_i64).unwrap(),
                Some(CountryCode::parse("GB").unwrap()),
            )
            .await
            .unwrap();
        assert!(!gb.providers[0].available);
        assert_eq!(gb.providers[0].kind, ProviderKind::Fake.as_str());
    }
}
