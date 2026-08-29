//! The `sanvi_http::LocaleResolver` implementation: negotiation order +
//! cached backends + catalog warming for the render hot path.

use std::sync::Arc;
use std::time::Duration;

use sanvi_http::{LocaleContext, LocaleResolutionInput, LocaleResolver};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::adapters::cache::LocalizationCache;
use crate::adapters::catalog::LocalizationService;
use crate::adapters::metrics::LocalizationMetrics;
use crate::config::LocalizationConfig;
use crate::domain::{
    Locale, LocaleSet, LocaleSource, NegotiationInput, SupportedLocaleRepository,
    TenantLocaleRepository, UserLocaleProvider, negotiate,
};

/// Cached negotiation: tenant settings, platform set and user preferences
/// all come from TTL caches; the merged catalogs are warmed before the
/// handler runs so `translate` stays synchronous.
pub struct LocaleResolverService {
    tx_manager: TxManager,
    tenant_locales: Arc<dyn TenantLocaleRepository>,
    supported: Arc<dyn SupportedLocaleRepository>,
    users: Arc<dyn UserLocaleProvider>,
    cache: Arc<LocalizationCache>,
    metrics: Arc<LocalizationMetrics>,
    service: Arc<LocalizationService>,
    config: LocalizationConfig,
}

impl LocaleResolverService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        tenant_locales: Arc<dyn TenantLocaleRepository>,
        supported: Arc<dyn SupportedLocaleRepository>,
        users: Arc<dyn UserLocaleProvider>,
        cache: Arc<LocalizationCache>,
        metrics: Arc<LocalizationMetrics>,
        service: Arc<LocalizationService>,
        config: LocalizationConfig,
    ) -> Self {
        Self {
            tx_manager,
            tenant_locales,
            supported,
            users,
            cache,
            metrics,
            service,
            config,
        }
    }

    async fn platform_set(&self) -> Arc<LocaleSet> {
        let ttl = Duration::from_secs(self.config.tenant_settings_cache_ttl_secs);
        if let Some(set) = self.cache.get_platform_set(ttl) {
            return set;
        }
        self.cache.record_miss();
        let set = self.load_platform_set().await;
        self.cache.set_platform_set(set.clone());
        set
    }

    async fn load_platform_set(&self) -> Arc<LocaleSet> {
        let default = Locale::parse(&self.config.default_locale).unwrap_or_default();
        match self.supported.list(self.tx_manager.pool()).await {
            Ok(locales) if !locales.is_empty() => {
                let supported: Vec<Locale> = locales.into_iter().map(|entry| entry.code).collect();
                let default = supported
                    .iter()
                    .find(|locale| locale.as_str() == default.as_str())
                    .cloned()
                    .unwrap_or_else(|| supported[0].clone());
                Arc::new(LocaleSet { supported, default })
            }
            _ => {
                // The table is seeded at migration time; this branch keeps
                // negotiation functional if the read fails.
                Arc::new(LocaleSet::single(default))
            }
        }
    }

    async fn tenant_settings(
        &self,
        tenant_id: TenantId,
    ) -> Option<crate::domain::TenantLocaleSettings> {
        let ttl = Duration::from_secs(self.config.tenant_settings_cache_ttl_secs);
        if let Some(settings) = self.cache.get_tenant_settings(tenant_id, ttl) {
            return settings;
        }
        self.cache.record_miss();
        let settings = match self.tx_manager.begin_for(tenant_id.as_i64()).await {
            Ok(mut scope) => match self.tenant_locales.get(&mut scope, tenant_id).await {
                Ok(settings) => {
                    let _ = scope.rollback().await;
                    settings
                }
                Err(err) => {
                    tracing::warn!(tenant_id = %tenant_id, error = %err, "tenant locale settings read failed; using tenant default");
                    let _ = scope.rollback().await;
                    None
                }
            },
            Err(err) => {
                tracing::warn!(tenant_id = %tenant_id, error = %err, "failed to open tenant scope for locale settings");
                None
            }
        };
        self.cache.set_tenant_settings(tenant_id, settings.clone());
        settings
    }

    async fn user_locale(&self, user_id: UserId) -> Option<String> {
        let ttl = Duration::from_secs(self.config.user_locale_cache_ttl_secs);
        if let Some(locale) = self.cache.get_user_locale(user_id, ttl) {
            return locale;
        }
        self.cache.record_miss();
        let locale = self.users.locale_for(user_id).await;
        self.cache.set_user_locale(user_id, locale.clone());
        locale
    }
}

fn map_source(source: LocaleSource) -> sanvi_http::LocaleSource {
    match source {
        LocaleSource::Explicit => sanvi_http::LocaleSource::Explicit,
        LocaleSource::UserPreference => sanvi_http::LocaleSource::UserPreference,
        LocaleSource::TenantDefault => sanvi_http::LocaleSource::TenantDefault,
        LocaleSource::AcceptLanguage => sanvi_http::LocaleSource::AcceptLanguage,
        LocaleSource::PlatformDefault => sanvi_http::LocaleSource::PlatformDefault,
    }
}

/// The conventional timezone for a locale when no tenant preference exists.
pub fn default_timezone_for(locale: &str) -> &'static str {
    if locale.starts_with("ja") {
        "Asia/Tokyo"
    } else {
        "UTC"
    }
}

impl LocaleResolver for LocaleResolverService {
    fn resolve<'a>(
        &'a self,
        input: LocaleResolutionInput<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = LocaleContext> + Send + 'a>> {
        Box::pin(async move {
            let started = std::time::Instant::now();

            let tenant_id = input.tenant.map(|tenant| tenant.tenant_id);
            let platform_set = self.platform_set().await;

            let settings = match tenant_id {
                Some(id) => self.tenant_settings(id).await,
                None => None,
            };

            // The tenant's supported set (falls back to the platform set).
            let supported: Arc<LocaleSet> = match &settings {
                Some(settings) => Arc::new(LocaleSet {
                    supported: settings.enabled_locales.clone(),
                    default: settings.default_locale.clone(),
                }),
                None => platform_set.clone(),
            };

            // The tenant default comes from localization settings when
            // present, otherwise the tenant context's phase-01 field.
            let tenant_default = settings
                .as_ref()
                .map(|settings| settings.default_locale.as_str().to_string())
                .or_else(|| input.tenant.map(|tenant| tenant.default_locale.clone()));

            let user_preference = match input.subject {
                Some(subject) => self.user_locale(subject.user_id).await,
                None => None,
            };

            let explicit = input.explicit_locale.map(str::to_string);
            let accept_language = input.accept_language.map(str::to_string);
            let explicit_supported = LocaleSet {
                supported: self
                    .config
                    .explicit_locale_allowlist
                    .iter()
                    .filter_map(|tag| Locale::parse(tag).ok())
                    .filter(|locale| platform_set.contains(locale))
                    .collect(),
                default: platform_set.default.clone(),
            };

            let negotiated = negotiate(&NegotiationInput {
                explicit: explicit.as_deref(),
                user_preference: user_preference.as_deref(),
                tenant_default: tenant_default.as_deref(),
                accept_language: accept_language.as_deref(),
                platform_default: platform_set.default.as_str(),
                supported: (*supported).clone(),
                platform_supported: explicit_supported,
            });

            let timezone = settings
                .as_ref()
                .map(|settings| settings.timezone.clone())
                .unwrap_or_else(|| default_timezone_for(negotiated.locale.as_str()).to_string());

            self.metrics
                .negotiated
                .with_label_values(&[negotiated.locale.as_str(), &negotiated.source.to_string()])
                .inc();
            self.metrics
                .resolution_duration
                .observe(started.elapsed().as_secs_f64());

            // Warm the merged catalogs for the whole fallback chain so the
            // handlers' synchronous translate calls hit the cache.
            let scope = tenant_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "platform".to_string());
            for candidate in &negotiated.fallback_chain {
                if let Err(err) = self.service.warm(&scope, candidate.as_str()).await {
                    tracing::warn!(error = %err, scope, locale = %candidate.as_str(), "failed to warm localization catalog");
                }
            }

            LocaleContext {
                locale: negotiated.locale.as_str().to_string(),
                source: map_source(negotiated.source),
                fallback_chain: negotiated
                    .fallback_chain
                    .iter()
                    .map(|locale| locale.as_str().to_string())
                    .collect(),
                timezone,
                tenant_id,
            }
        })
    }
}
