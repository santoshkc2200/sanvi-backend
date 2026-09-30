//! Platform registry construction and access token provider wiring.

use std::collections::HashMap;
use std::sync::Arc;

use sanvi_persistence::TxManager;

use sanvi_kernel::CapabilityMode;

use crate::adapters::{
    FakeAdPlatform, FakeAudienceSync, FakeConversionSink, GoogleAdsAdapter, GoogleAudienceSync,
    GoogleConversionSink, GoogleRefreshTokenGrant, MetaAdsAdapter, MetaAudienceSync,
    MetaConversionSink, MetaRefreshTokenGrant,
};
use crate::application::{PlatformRegistry, VaultAccessTokenProvider};
use crate::config::AdvertisingConfig;
use crate::domain::{
    AdvertisingError, AudienceSyncPort, ConnectionAccessTokenProvider, ConversionSinkPort,
    PlatformKey, TokenVault,
};

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
/// Under `CapabilityMode::Disabled`, this returns an empty platform registry.
/// Under `CapabilityMode::Sandbox`, this returns the fake in-process platform catalog.
/// Under `CapabilityMode::Live`, this attempts to construct and register the real adapter
/// for each supported platform (`google_ads`, `meta`). In live mode, a missing token provider
/// or unconstructable adapter for a configured platform is a hard error — never a silent fallback to a fake.
pub fn build_platform_registry(
    config: &AdvertisingConfig,
    token_providers: &HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>>,
) -> Result<(PlatformRegistry, CapabilityMode), AdvertisingError> {
    let mode = config
        .resolved_platform_mode()
        .map_err(|err| AdvertisingError::InvalidInput(err.to_string()))?;
    match mode {
        CapabilityMode::Disabled => {
            tracing::info!("advertising platform registry: disabled mode");
            Ok((PlatformRegistry::new(), CapabilityMode::Disabled))
        }
        CapabilityMode::Sandbox => {
            tracing::info!("advertising platform registry: sandbox mode (using fixtures)");
            Ok((
                FakeAdPlatform::registered_catalog(),
                CapabilityMode::Sandbox,
            ))
        }
        CapabilityMode::Live => {
            tracing::info!("advertising platform registry: live mode");
            let mut registry = PlatformRegistry::new();

            // Google Ads
            let google_key = PlatformKey::parse("google_ads").expect("static platform key");
            if config.google_ads_client_id.is_empty() {
                tracing::info!(
                    "google ads client id not configured; google ads capability disabled"
                );
            } else {
                let Some(provider) = token_providers.get(&google_key) else {
                    return Err(AdvertisingError::PlatformUnavailable(
                        "google ads token provider not available in live mode".to_string(),
                    ));
                };
                let adapter = GoogleAdsAdapter::new(config, provider.clone())?;
                registry
                    .register(Arc::new(adapter))
                    .expect("distinct platform key registration cannot fail");
            }

            // Meta
            let meta_key = PlatformKey::parse("meta").expect("static platform key");
            if config.meta_client_id.is_empty() {
                tracing::info!("meta client id not configured; meta capability disabled");
            } else {
                let Some(provider) = token_providers.get(&meta_key) else {
                    return Err(AdvertisingError::PlatformUnavailable(
                        "meta token provider not available in live mode".to_string(),
                    ));
                };
                let adapter = MetaAdsAdapter::new(config, provider.clone())?;
                registry
                    .register(Arc::new(adapter))
                    .expect("distinct platform key registration cannot fail");
            }

            Ok((registry, CapabilityMode::Live))
        }
    }
}

/// Construct conversion sinks for advertising. Live mode builds the real
/// Google/Meta sinks per platform whose token provider exists, mirroring
/// `build_platform_registry`'s per-platform fallback: a missing provider
/// means that platform has no sink rather than a failed process.
pub fn build_conversion_sinks(
    config: &AdvertisingConfig,
    token_providers: &HashMap<PlatformKey, std::sync::Arc<dyn ConnectionAccessTokenProvider>>,
) -> Result<
    (
        std::collections::BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>>,
        CapabilityMode,
    ),
    AdvertisingError,
> {
    let mode = config
        .resolved_conversion_sink_mode()
        .map_err(|err| AdvertisingError::InvalidInput(err.to_string()))?;
    match mode {
        CapabilityMode::Disabled => {
            Ok((std::collections::BTreeMap::new(), CapabilityMode::Disabled))
        }
        CapabilityMode::Sandbox => {
            let mut sinks: std::collections::BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> =
                std::collections::BTreeMap::new();
            for descriptor in FakeAdPlatform::fixtures().into_iter().take(2) {
                sinks.insert(
                    descriptor.key.clone(),
                    Arc::new(FakeConversionSink::new(descriptor)),
                );
            }
            Ok((sinks, CapabilityMode::Sandbox))
        }
        CapabilityMode::Live => {
            let mut sinks: std::collections::BTreeMap<PlatformKey, Arc<dyn ConversionSinkPort>> =
                std::collections::BTreeMap::new();
            if let Some(provider) = token_providers.get(&PlatformKey::parse("google_ads")?) {
                sinks.insert(
                    PlatformKey::parse("google_ads")?,
                    Arc::new(GoogleConversionSink::new(config, provider.clone())?),
                );
            }
            if let Some(provider) = token_providers.get(&PlatformKey::parse("meta")?) {
                sinks.insert(
                    PlatformKey::parse("meta")?,
                    Arc::new(MetaConversionSink::new(config, provider.clone())?),
                );
            }
            Ok((sinks, CapabilityMode::Live))
        }
    }
}

/// Construct audience sync sinks for advertising (same per-platform fallback
/// as [`build_conversion_sinks`]).
pub fn build_audience_sinks(
    config: &AdvertisingConfig,
    token_providers: &HashMap<PlatformKey, std::sync::Arc<dyn ConnectionAccessTokenProvider>>,
) -> Result<
    (
        std::collections::BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>>,
        CapabilityMode,
    ),
    AdvertisingError,
> {
    let mode = config
        .resolved_audience_sink_mode()
        .map_err(|err| AdvertisingError::InvalidInput(err.to_string()))?;
    match mode {
        CapabilityMode::Disabled => {
            Ok((std::collections::BTreeMap::new(), CapabilityMode::Disabled))
        }
        CapabilityMode::Sandbox => {
            let mut sinks: std::collections::BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>> =
                std::collections::BTreeMap::new();
            for descriptor in FakeAdPlatform::fixtures().into_iter().take(2) {
                sinks.insert(
                    descriptor.key.clone(),
                    Arc::new(FakeAudienceSync::new(descriptor)),
                );
            }
            Ok((sinks, CapabilityMode::Sandbox))
        }
        CapabilityMode::Live => {
            let mut sinks: std::collections::BTreeMap<PlatformKey, Arc<dyn AudienceSyncPort>> =
                std::collections::BTreeMap::new();
            if let Some(provider) = token_providers.get(&PlatformKey::parse("google_ads")?) {
                sinks.insert(
                    PlatformKey::parse("google_ads")?,
                    Arc::new(GoogleAudienceSync::with_custom_endpoints(
                        config,
                        crate::adapters::google::DEFAULT_API_BASE.to_string(),
                        provider.clone(),
                    )?),
                );
            }
            if let Some(provider) = token_providers.get(&PlatformKey::parse("meta")?) {
                sinks.insert(
                    PlatformKey::parse("meta")?,
                    Arc::new(MetaAudienceSync::with_custom_endpoints(
                        config,
                        format!("https://graph.facebook.com/{}", config.meta_api_version),
                        provider.clone(),
                    )?),
                );
            }
            Ok((sinks, CapabilityMode::Live))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AdvertisingError, ConnectionHandle};
    use async_trait::async_trait;
    use sanvi_telemetry::Redacted;

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
        let (registry, mode) = build_platform_registry(&config, &token_providers).unwrap();
        assert_eq!(mode, CapabilityMode::Sandbox);
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
    fn explicit_disabled_mode_returns_empty_registry() {
        let config = AdvertisingConfig {
            platform_mode: Some("disabled".to_string()),
            ..AdvertisingConfig::default()
        };
        let token_providers = HashMap::new();
        let (registry, mode) = build_platform_registry(&config, &token_providers).unwrap();
        assert_eq!(mode, CapabilityMode::Disabled);
        assert_eq!(registry.adapters().len(), 0);
    }

    #[test]
    fn flag_true_empty_credentials_disables_capabilities_without_fake_fallback() {
        let config = AdvertisingConfig {
            real_platform_adapters: true,
            ..AdvertisingConfig::default()
        };
        let token_providers = HashMap::new();
        let (registry, mode) = build_platform_registry(&config, &token_providers).unwrap();
        assert_eq!(mode, CapabilityMode::Live);
        // Neither platform configured, so neither registered (no fake fallback!)
        assert_eq!(registry.adapters().len(), 0);
    }

    #[test]
    fn flag_true_missing_token_provider_fails_startup() {
        let config = AdvertisingConfig {
            real_platform_adapters: true,
            google_ads_client_id: "google-client-id".to_string(),
            ..AdvertisingConfig::default()
        };
        let token_providers = HashMap::new();
        match build_platform_registry(&config, &token_providers) {
            Err(err) => assert!(matches!(err, AdvertisingError::PlatformUnavailable(_))),
            Ok(_) => panic!("expected failure in live mode with missing token provider"),
        }
    }

    #[test]
    fn flag_true_missing_developer_token_fails_startup() {
        let config = AdvertisingConfig {
            real_platform_adapters: true,
            google_ads_client_id: "google-client-id".to_string(),
            google_ads_developer_token: Redacted::default(),
            ..AdvertisingConfig::default()
        };
        let mut token_providers: HashMap<PlatformKey, Arc<dyn ConnectionAccessTokenProvider>> =
            HashMap::new();
        let google_key = PlatformKey::parse("google_ads").unwrap();
        token_providers.insert(google_key, Arc::new(StubTokenProvider));

        match build_platform_registry(&config, &token_providers) {
            Err(err) => assert!(matches!(err, AdvertisingError::PlatformUnavailable(_))),
            Ok(_) => panic!("expected failure in live mode with missing developer token"),
        }
    }

    #[test]
    fn sinks_builders_modes() {
        let no_providers = HashMap::new();
        let sandbox_config = AdvertisingConfig {
            real_platform_adapters: false,
            ..AdvertisingConfig::default()
        };
        let (conv_sinks, conv_mode) =
            build_conversion_sinks(&sandbox_config, &no_providers).unwrap();
        assert_eq!(conv_mode, CapabilityMode::Sandbox);
        assert_eq!(conv_sinks.len(), 2);

        let (aud_sinks, aud_mode) = build_audience_sinks(&sandbox_config, &no_providers).unwrap();
        assert_eq!(aud_mode, CapabilityMode::Sandbox);
        assert_eq!(aud_sinks.len(), 2);

        let live_config = AdvertisingConfig {
            real_platform_adapters: true,
            ..AdvertisingConfig::default()
        };
        // Live mode with no configured token providers yields no sinks — the
        // per-platform fallback never fails the process, it just skips that
        // platform (same rule as `build_platform_registry`).
        let (conv_live, conv_live_mode) =
            build_conversion_sinks(&live_config, &no_providers).unwrap();
        assert_eq!(conv_live_mode, CapabilityMode::Live);
        assert_eq!(conv_live.len(), 0);

        let (aud_live, aud_live_mode) = build_audience_sinks(&live_config, &no_providers).unwrap();
        assert_eq!(aud_live_mode, CapabilityMode::Live);
        assert_eq!(aud_live.len(), 0);
    }
}
