//! Platform registry construction and access token provider wiring.

use std::collections::HashMap;
use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::adapters::{
    FakeAdPlatform, GoogleAdsAdapter, GoogleRefreshTokenGrant, MetaAdsAdapter,
    MetaRefreshTokenGrant,
};
use crate::application::{PlatformRegistry, VaultAccessTokenProvider};
use crate::config::AdvertisingConfig;
use crate::domain::{ConnectionAccessTokenProvider, PlatformKey, TokenVault};

/// Build connection access token providers for platforms whose OAuth client
/// credentials are configured.
pub fn build_access_token_providers(
    config: &AdvertisingConfig,
    tx_manager: TxManager,
    token_vault: Arc<TokenVault>,
) -> HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>> {
    let mut providers: HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>> =
        HashMap::new();

    if config.google_ads_client_id.is_empty() {
        tracing::info!("advertising token refresh: google oauth client not configured, skipping");
    } else {
        match GoogleRefreshTokenGrant::new(config) {
            Ok(grant) => {
                providers.insert(
                    PlatformKey::parse("google_ads").expect("static platform key"),
                    Arc::new(VaultAccessTokenProvider::new(
                        tx_manager.clone(),
                        token_vault.clone(),
                        Arc::new(grant),
                    )),
                );
            }
            Err(error) => {
                tracing::error!(%error, "advertising token refresh: google grant unavailable");
            }
        }
    }

    if config.meta_client_id.is_empty() {
        tracing::info!("advertising token refresh: meta oauth client not configured, skipping");
    } else {
        match MetaRefreshTokenGrant::new(config) {
            Ok(grant) => {
                providers.insert(
                    PlatformKey::parse("meta").expect("static platform key"),
                    Arc::new(VaultAccessTokenProvider::new(
                        tx_manager,
                        token_vault,
                        Arc::new(grant),
                    )),
                );
            }
            Err(error) => {
                tracing::error!(%error, "advertising token refresh: meta grant unavailable");
            }
        }
    }

    providers
}

/// Construct the platform registry for advertising.
///
/// If `config.real_platform_adapters` is false, this returns the fake in-process
/// platform catalog.
/// If `config.real_platform_adapters` is true, this attempts to construct and
/// register the real adapter for each supported platform (`google_ads`, `meta`).
/// If a platform's token provider is missing or adapter construction fails (e.g.
/// missing credentials), it logs a warning and falls back to that platform's fake
/// adapter fixture.
pub fn build_platform_registry(
    config: &AdvertisingConfig,
    token_providers: &HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>>,
) -> PlatformRegistry {
    if !config.real_platform_adapters {
        return FakeAdPlatform::registered_catalog();
    }

    let mut registry = PlatformRegistry::new();

    // Google Ads
    let google_key = PlatformKey::parse("google_ads").expect("static platform key");
    if let Some(provider) = token_providers.get(&google_key) {
        match GoogleAdsAdapter::new(config, provider.clone()) {
            Ok(adapter) => {
                registry
                    .register(Arc::new(adapter))
                    .expect("distinct platform key registration cannot fail");
            }
            Err(error) => {
                tracing::warn!(%error, "google ads adapter unavailable, falling back to fake platform");
                let descriptor = FakeAdPlatform::fixtures()
                    .into_iter()
                    .find(|d| d.key == google_key)
                    .expect("fake descriptor for google_ads fixture exists");
                registry
                    .register(Arc::new(FakeAdPlatform::new(descriptor)))
                    .expect("distinct platform key registration cannot fail");
            }
        }
    } else {
        tracing::warn!("google ads token provider not available, falling back to fake platform");
        let descriptor = FakeAdPlatform::fixtures()
            .into_iter()
            .find(|d| d.key == google_key)
            .expect("fake descriptor for google_ads fixture exists");
        registry
            .register(Arc::new(FakeAdPlatform::new(descriptor)))
            .expect("distinct platform key registration cannot fail");
    }

    // Meta
    let meta_key = PlatformKey::parse("meta").expect("static platform key");
    if let Some(provider) = token_providers.get(&meta_key) {
        match MetaAdsAdapter::new(config, provider.clone()) {
            Ok(adapter) => {
                registry
                    .register(Arc::new(adapter))
                    .expect("distinct platform key registration cannot fail");
            }
            Err(error) => {
                tracing::warn!(%error, "meta ads adapter unavailable, falling back to fake platform");
                let descriptor = FakeAdPlatform::fixtures()
                    .into_iter()
                    .find(|d| d.key == meta_key)
                    .expect("fake descriptor for meta fixture exists");
                registry
                    .register(Arc::new(FakeAdPlatform::new(descriptor)))
                    .expect("distinct platform key registration cannot fail");
            }
        }
    } else {
        tracing::warn!("meta token provider not available, falling back to fake platform");
        let descriptor = FakeAdPlatform::fixtures()
            .into_iter()
            .find(|d| d.key == meta_key)
            .expect("fake descriptor for meta fixture exists");
        registry
            .register(Arc::new(FakeAdPlatform::new(descriptor)))
            .expect("distinct platform key registration cannot fail");
    }

    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AdvertisingError, ConnectionHandle};
    use async_trait::async_trait;

    struct StubTokenProvider;

    #[async_trait]
    impl ConnectionAccessTokenProvider for StubTokenProvider {
        async fn access_token(
            &self,
            _connection: &ConnectionHandle,
        ) -> Result<String, AdvertisingError> {
            Ok("test-token".to_string())
        }
    }

    #[test]
    fn flag_false_returns_fake_registered_catalog() {
        let config = AdvertisingConfig {
            real_platform_adapters: false,
            ..AdvertisingConfig::default()
        };
        let token_providers = HashMap::new();
        let registry = build_platform_registry(&config, &token_providers);
        let expected: std::collections::BTreeSet<_> = FakeAdPlatform::fixtures()
            .into_iter()
            .take(2)
            .map(|d| d.key)
            .collect();
        let actual: std::collections::BTreeSet<_> = registry
            .adapters()
            .iter()
            .map(|a| a.descriptor().key)
            .collect();
        assert_eq!(expected, actual);
    }

    #[test]
    fn flag_true_empty_credentials_falls_back_to_fakes_for_both_platforms() {
        let config = AdvertisingConfig {
            real_platform_adapters: true,
            ..AdvertisingConfig::default()
        };
        let token_providers = HashMap::new();
        let registry = build_platform_registry(&config, &token_providers);

        let google_key = PlatformKey::parse("google_ads").unwrap();
        let meta_key = PlatformKey::parse("meta").unwrap();

        assert!(registry.adapter(&google_key).is_ok());
        assert!(registry.adapter(&meta_key).is_ok());
        assert_eq!(registry.adapters().len(), 2);
    }

    #[test]
    fn flag_true_partial_credentials_falls_back_cleanly() {
        let config = AdvertisingConfig {
            real_platform_adapters: true,
            google_ads_developer_token: String::new(),
            ..AdvertisingConfig::default()
        };
        let mut token_providers: HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>> =
            HashMap::new();
        let google_key = PlatformKey::parse("google_ads").unwrap();
        token_providers.insert(google_key.clone(), Arc::new(StubTokenProvider));

        let registry = build_platform_registry(&config, &token_providers);

        let meta_key = PlatformKey::parse("meta").unwrap();

        assert!(registry.adapter(&google_key).is_ok());
        assert!(registry.adapter(&meta_key).is_ok());
        assert_eq!(registry.adapters().len(), 2);
    }
}
