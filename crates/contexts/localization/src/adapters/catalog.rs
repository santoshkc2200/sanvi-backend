//! Catalog resolution: static catalogs (embedded at compile time) merged
//! with platform and tenant overrides, cached per (scope, locale).
//!
//! The hot path ([`LocalizationService::translate`]) is synchronous and
//! never touches the database: it renders from the cached merged snapshot,
//! falling back to the static catalog when the cache is cold or expired.
//! The async [`LocalizationService::warm`] loads and merges; middleware and
//! workers call it before rendering so the snapshot is hot by the time
//! handlers run.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};


use sanvi_persistence::{PgPool, TxManager};
use sanvi_kernel::TenantId;

use crate::adapters::cache::LocalizationCache;
use crate::adapters::metrics::LocalizationMetrics;
use crate::config::LocalizationConfig;
use crate::domain::{
    Arg, Locale, LocalizationError, Segment, Translation, TranslationRepository, Translator,
    message_format::{parse_pattern_with_key, render},
};

/// A merged catalog for one (scope, locale): raw strings (for API
/// delivery) plus pre-parsed patterns (for the render hot path).
#[derive(Debug, Default)]
pub struct MergedCatalog {
    pub patterns: HashMap<String, Arc<Vec<Segment>>>,
    pub raw: BTreeMap<String, String>,
}

/// The parsed static catalogs, loaded once.
struct StaticCatalogs {
    /// (locale, domain) → parsed catalog.
    by_locale: HashMap<String, HashMap<String, ParsedCatalog>>,
}

struct ParsedCatalog {
    patterns: HashMap<String, Arc<Vec<Segment>>>,
    raw: BTreeMap<String, String>,
}

fn static_catalogs() -> &'static StaticCatalogs {
    static CATALOGS: OnceLock<StaticCatalogs> = OnceLock::new();
    CATALOGS.get_or_init(|| {
        let mut by_locale: HashMap<String, HashMap<String, ParsedCatalog>> = HashMap::new();
        for (locale, domain, content) in crate::generated::CATALOG_FILES {
            let map: BTreeMap<String, String> =
                serde_json::from_str(content).unwrap_or_else(|err| {
                    panic!("embedded catalog {locale}/{domain} is invalid: {err}")
                });
            let mut parsed = ParsedCatalog {
                patterns: HashMap::new(),
                raw: map.clone(),
            };
            for (key, pattern) in &map {
                let segments = parse_pattern_with_key(key, pattern).unwrap_or_else(|err| {
                    panic!("embedded catalog {locale}/{domain} key {key} does not parse: {err}")
                });
                parsed.patterns.insert(key.clone(), Arc::new(segments));
            }
            by_locale
                .entry(locale.to_string())
                .or_default()
                .insert(domain.to_string(), parsed);
        }
        StaticCatalogs { by_locale }
    })
}

/// The shared localization service: the `Translator` port implementation,
/// the merge cache and the `ProblemTranslator` used by the HTTP error
/// layer.
pub struct LocalizationService {
    pool: PgPool,
    tx_manager: TxManager,
    translations: Arc<dyn TranslationRepository>,
    cache: Arc<LocalizationCache>,
    metrics: Arc<LocalizationMetrics>,
    config: LocalizationConfig,
}

impl LocalizationService {
    pub fn new(
        pool: PgPool,
        ids: Arc<dyn sanvi_kernel::RawIdGenerator>,
        translations: Arc<dyn TranslationRepository>,
        cache: Arc<LocalizationCache>,
        metrics: Arc<LocalizationMetrics>,
        config: LocalizationConfig,
    ) -> Self {
        let tx_manager = TxManager::new(pool.clone(), ids);
        Self {
            pool,
            tx_manager,
            translations,
            cache,
            metrics,
            config,
        }
    }

    pub fn cache(&self) -> &Arc<LocalizationCache> {
        &self.cache
    }

    pub fn metrics(&self) -> &Arc<LocalizationMetrics> {
        &self.metrics
    }

    /// Load (or reload) the merged catalog for a scope + locale. Called by
    /// the negotiation middleware, handlers that render, and worker jobs —
    /// never by the synchronous translate path.
    pub async fn warm(&self, scope: &str, locale: &str) -> Result<(), LocalizationError> {
        if self.cached_merged(scope, locale).is_some() {
            return Ok(());
        }
        let merged = self.build_merged(scope, locale).await?;
        self.cache.set_merged(scope, locale, Arc::new(merged));
        Ok(())
    }

    /// Invalidate one scope's merged snapshots (also published as an event
    /// so other processes do the same).
    pub fn invalidate_scope(&self, scope: &str) {
        if scope == "platform" {
            self.cache.invalidate_all_merged();
        } else {
            self.cache.invalidate_scope(scope);
        }
    }

    pub fn invalidate_tenant(&self, tenant_id: TenantId) {
        self.cache.invalidate_tenant(tenant_id);
    }

    async fn build_merged(
        &self,
        scope: &str,
        locale: &str,
    ) -> Result<MergedCatalog, LocalizationError> {
        let statics = static_catalogs();
        let mut merged = MergedCatalog::default();

        // 1. Static catalog (all shipped domains for this locale).
        if let Some(domains) = statics.by_locale.get(locale) {
            for catalog in domains.values() {
                merged.raw.extend(catalog.raw.clone());
                merged
                    .patterns
                    .extend(catalog.patterns.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }

        // 2. Platform overrides (public copy, readable without a scope).
        let platform_overrides = self.translations.list_platform(&self.pool).await?;
        for translation in platform_overrides
            .into_iter()
            .filter(|translation| translation.locale == locale)
        {
            apply_override(&mut merged, &translation)?;
        }

        // 3. Tenant overrides (scope is a tenant id).
        if scope != "platform"
            && let Ok(tenant_id) = scope.parse::<i64>()
        {
            let mut scoped = self.tx_manager.begin_for(tenant_id).await.map_err(|err| {
                LocalizationError::Storage(format!("failed to open tenant scope: {err}"))
            })?;
            let tenant_id = TenantId::try_from(tenant_id).map_err(|err| {
                LocalizationError::Storage(format!("invalid tenant id in scope: {err}"))
            })?;
            let tenant_overrides = self
                .translations
                .list_tenant(&mut scoped, tenant_id)
                .await?;
            scoped.rollback().await.map_err(|err| {
                LocalizationError::Storage(format!("failed to close tenant scope: {err}"))
            })?;
            for translation in tenant_overrides
                .into_iter()
                .filter(|translation| translation.locale == locale)
            {
                apply_override(&mut merged, &translation)?;
            }
        }

        Ok(merged)
    }

    /// Render from the cached snapshot. Cold/expired cache renders the
    /// static catalog (correct fallback) and records a miss — the hot path
    /// never blocks on I/O.
    fn translate_cached(
        &self,
        scope: &str,
        locale: &str,
        timezone: &str,
        key: &str,
        args: &[(&str, Arg)],
    ) -> Option<String> {
        let merged = self.cached_merged(scope, locale)?;
        render_from(&merged, key, args, locale, timezone)
    }

    fn cached_merged(&self, scope: &str, locale: &str) -> Option<Arc<MergedCatalog>> {
        let ttl = std::time::Duration::from_secs(self.config.merge_cache_ttl_secs);
        match self.cache.get_merged(scope, locale, ttl) {
            Some(merged) => {
                self.metrics.cache_hits.inc();
                Some(merged)
            }
            None => {
                self.cache.record_miss();
                self.metrics.cache_misses.inc();
                None
            }
        }
    }

    fn translate_static(
        &self,
        locale: &str,
        timezone: &str,
        key: &str,
        args: &[(&str, Arg)],
    ) -> String {
        let statics = static_catalogs();
        for (_, domain) in statics.by_locale.get(locale).into_iter().flatten() {
            if let Some(segments) = domain.patterns.get(key) {
                return render_with_policy(segments, args, locale, timezone, key);
            }
        }
        // Final guard: the t! macro guarantees this cannot happen for
        // compile-time-checked keys.
        self.metrics
            .missing_key
            .with_label_values(&[key, locale])
            .inc();
        key.to_string()
    }
}

fn apply_override(
    merged: &mut MergedCatalog,
    translation: &Translation,
) -> Result<(), LocalizationError> {
    let segments = parse_pattern_with_key(&translation.key, &translation.value)?;
    merged
        .patterns
        .insert(translation.key.clone(), Arc::new(segments));
    merged
        .raw
        .insert(translation.key.clone(), translation.value.clone());
    Ok(())
}

/// Look a key up in a merged snapshot, walking the locale fallback chain
/// handled by the caller via successive snapshots; here a miss is a miss.
fn render_from(
    merged: &MergedCatalog,
    key: &str,
    args: &[(&str, Arg)],
    locale: &str,
    timezone: &str,
) -> Option<String> {
    let segments = merged.patterns.get(key)?;
    Some(render_with_policy(segments, args, locale, timezone, key))
}

fn render_with_policy(
    segments: &[Segment],
    args: &[(&str, Arg)],
    locale: &str,
    timezone: &str,
    key: &str,
) -> String {
    let policy = crate::domain::FormattingPolicy::for_locale(
        Locale::parse(locale).unwrap_or_default(),
        timezone,
    )
    .unwrap_or_else(|_| {
        crate::domain::FormattingPolicy::for_locale(Locale::default(), "UTC")
            .expect("default policy is valid")
    });
    let map: HashMap<String, Arg> = args
        .iter()
        .map(|(name, arg)| (name.to_string(), arg.clone()))
        .collect();
    let rendered = render(segments, &map, &policy);
    if rendered.is_empty() {
        // Never return an empty string from translation.
        return key.to_string();
    }
    rendered
}

fn locale_timezone(locale: &str) -> &'static str {
    if locale.starts_with("ja") {
        "Asia/Tokyo"
    } else {
        "UTC"
    }
}

impl Translator for LocalizationService {
    fn translate(&self, scope: &str, locale: &str, key: &str, args: &[(&str, Arg)]) -> String {
        self.translate_with_timezone(scope, locale, locale_timezone(locale), key, args)
    }

    fn translate_with_timezone(
        &self,
        scope: &str,
        locale: &str,
        timezone: &str,
        key: &str,
        args: &[(&str, Arg)],
    ) -> String {
        // Walk the fallback chain: most specific locale first, ending in
        // the source locale (en).
        let mut tried: Vec<&str> = Vec::new();
        let chain = fallback_chain(locale);
        for candidate in &chain {
            if tried.contains(&candidate.as_str()) {
                continue;
            }
            tried.push(candidate.as_str());
            if let Some(rendered) = self.translate_cached(scope, candidate, timezone, key, args) {
                return rendered;
            }
        }
        // Static fallback (never touches the DB, never empty).
        for candidate in &chain {
            let rendered = self.translate_static(candidate, timezone, key, args);
            if !rendered.is_empty() && rendered != key {
                return rendered;
            }
        }
        self.translate_static("en", timezone, key, args)
    }
}

fn fallback_chain(locale: &str) -> Vec<String> {
    let mut chain = vec![locale.to_string()];
    if let Some((language, _)) = locale.split_once('-')
        && language != locale
    {
        chain.push(language.to_string());
    }
    if !chain.iter().any(|entry| entry == "en") {
        chain.push("en".to_string());
    }
    chain
}

impl sanvi_http::ProblemTranslator for LocalizationService {
    fn title(&self, type_slug: &str, scope: &str, locale: &str) -> Option<String> {
        use crate::t;
        let key = match type_slug {
            "bad-request" => t!("errors.title.bad-request"),
            "unauthorized" => t!("errors.title.unauthorized"),
            "forbidden" => t!("errors.title.forbidden"),
            "not-found" => t!("errors.title.not-found"),
            "conflict" => t!("errors.title.conflict"),
            "gone" => t!("errors.title.gone"),
            "bad-gateway" => t!("errors.title.bad-gateway"),
            "tenant-suspended" => t!("errors.title.tenant-suspended"),
            "too-many-requests" => t!("errors.title.too-many-requests"),
            "request-timeout" => t!("errors.title.request-timeout"),
            "service-unavailable" => t!("errors.title.service-unavailable"),
            "internal-error" => t!("errors.title.internal-error"),
            "error" => t!("errors.title.error"),
            _ => return None,
        };
        let rendered = self.translate(scope, locale, key.key, &[]);
        (rendered != key.key && !rendered.is_empty()).then_some(rendered)
    }

    fn detail(&self, type_slug: &str, scope: &str, locale: &str) -> Option<String> {
        use crate::t;
        let key = match type_slug {
            "bad-request" => t!("errors.detail.bad-request"),
            "unauthorized" => t!("errors.detail.unauthorized"),
            "forbidden" => t!("errors.detail.forbidden"),
            "not-found" => t!("errors.detail.not-found"),
            "conflict" => t!("errors.detail.conflict"),
            "gone" => t!("errors.detail.gone"),
            "bad-gateway" => t!("errors.detail.bad-gateway"),
            "tenant-suspended" => t!("errors.detail.tenant-suspended"),
            "too-many-requests" => t!("errors.detail.too-many-requests"),
            "request-timeout" => t!("errors.detail.request-timeout"),
            "service-unavailable" => t!("errors.detail.service-unavailable"),
            "internal-error" => t!("errors.detail.internal-error"),
            "error" => t!("errors.detail.error"),
            _ => return None,
        };
        let rendered = self.translate(scope, locale, key.key, &[]);
        (rendered != key.key && !rendered.is_empty()).then_some(rendered)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::{TimeZone, Utc};
    use sanvi_persistence::{PlatformScope, ScopedTx};

    use super::*;

    struct CatalogRepo {
        platform_reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TranslationRepository for CatalogRepo {
        async fn list_tenant(
            &self,
            _scope: &mut ScopedTx<'_>,
            _tenant_id: TenantId,
        ) -> Result<Vec<Translation>, LocalizationError> {
            Ok(Vec::new())
        }

        async fn list_platform(
            &self,
            _pool: &PgPool,
        ) -> Result<Vec<Translation>, LocalizationError> {
            self.platform_reads.fetch_add(1, Ordering::Relaxed);
            Ok(vec![
                translation("en", "Invoice override"),
                translation("ja", "請求書の上書き"),
            ])
        }

        async fn list_platform_scoped(
            &self,
            _scope: &mut PlatformScope<'_>,
        ) -> Result<Vec<Translation>, LocalizationError> {
            Ok(Vec::new())
        }

        async fn upsert_tenant(
            &self,
            _scope: &mut ScopedTx<'_>,
            _tenant_id: TenantId,
            _translation: &Translation,
        ) -> Result<(), LocalizationError> {
            Ok(())
        }

        async fn upsert_platform(
            &self,
            _scope: &mut PlatformScope<'_>,
            _translation: &Translation,
        ) -> Result<(), LocalizationError> {
            Ok(())
        }

        async fn delete_tenant(
            &self,
            _scope: &mut ScopedTx<'_>,
            _tenant_id: TenantId,
            _key: &str,
            _locale: &str,
        ) -> Result<(), LocalizationError> {
            Ok(())
        }

        async fn delete_platform(
            &self,
            _scope: &mut PlatformScope<'_>,
            _key: &str,
            _locale: &str,
        ) -> Result<(), LocalizationError> {
            Ok(())
        }
    }

    fn translation(locale: &str, value: &str) -> Translation {
        Translation {
            scope: "platform".to_string(),
            key: "billing.invoice.label".to_string(),
            locale: locale.to_string(),
            value: value.to_string(),
            updated_by: None,
            updated_at: Utc::now(),
        }
    }

    fn service(repo: Arc<CatalogRepo>) -> LocalizationService {
        let pool = PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects");
        LocalizationService::new(
            pool,
            sanvi_testsupport::test_ids(),
            repo,
            Arc::new(LocalizationCache::new()),
            Arc::new(LocalizationMetrics::new(&prometheus::Registry::new())),
            LocalizationConfig::default(),
        )
    }

    #[tokio::test]
    async fn merge_applies_only_overrides_for_the_requested_locale() {
        let repo = Arc::new(CatalogRepo {
            platform_reads: AtomicUsize::new(0),
        });
        let service = service(repo);
        service.warm("platform", "en").await.unwrap();
        assert_eq!(
            service.translate("platform", "en", "billing.invoice.label", &[]),
            "Invoice override"
        );
    }

    #[tokio::test]
    async fn warm_reuses_fresh_catalog_and_records_prometheus_cache_metrics() {
        let repo = Arc::new(CatalogRepo {
            platform_reads: AtomicUsize::new(0),
        });
        let service = service(repo.clone());
        service.warm("platform", "en").await.unwrap();
        service.warm("platform", "en").await.unwrap();
        assert_eq!(repo.platform_reads.load(Ordering::Relaxed), 1);
        assert_eq!(service.metrics().cache_misses.get(), 1);
        assert!(service.metrics().cache_hits.get() >= 1);
    }

    #[tokio::test]
    async fn rendering_uses_the_subject_timezone_when_supplied() {
        let repo = Arc::new(CatalogRepo {
            platform_reads: AtomicUsize::new(0),
        });
        let service = service(repo);
        let date = Utc.with_ymd_and_hms(2026, 8, 15, 23, 30, 0).unwrap();
        let rendered = service.translate_with_timezone(
            "platform",
            "en",
            "Asia/Tokyo",
            "billing.invoice.due_date",
            &[("date", Arg::from(date))],
        );
        assert_eq!(rendered, "Due Aug 16, 2026");
    }
}
