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

use uuid::Uuid;

use sanvi_persistence::{PgPool, TxManager};

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
        translations: Arc<dyn TranslationRepository>,
        cache: Arc<LocalizationCache>,
        metrics: Arc<LocalizationMetrics>,
        config: LocalizationConfig,
    ) -> Self {
        let tx_manager = TxManager::new(pool.clone());
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
        let merged = self.build_merged(scope, locale).await?;
        self.cache.set_merged(scope, locale, Arc::new(merged));
        Ok(())
    }

    /// Invalidate one scope's merged snapshots (also published as an event
    /// so other processes do the same).
    pub fn invalidate_scope(&self, scope: &str) {
        self.cache.invalidate_scope(scope);
    }

    pub fn invalidate_tenant(&self, tenant_id: Uuid) {
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
        for translation in platform_overrides {
            apply_override(&mut merged, &translation)?;
        }

        // 3. Tenant overrides (scope is a tenant id).
        if scope != "platform"
            && let Ok(tenant_id) = scope.parse::<Uuid>()
        {
            let mut scoped = self.tx_manager.begin_for(tenant_id).await.map_err(|err| {
                LocalizationError::Storage(format!("failed to open tenant scope: {err}"))
            })?;
            let tenant_overrides = self
                .translations
                .list_tenant(&mut scoped, tenant_id)
                .await?;
            scoped.rollback().await.map_err(|err| {
                LocalizationError::Storage(format!("failed to close tenant scope: {err}"))
            })?;
            for translation in tenant_overrides {
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
        key: &str,
        args: &[(&str, Arg)],
    ) -> Option<String> {
        let ttl = std::time::Duration::from_secs(self.config.merge_cache_ttl_secs);
        let merged = self.cache.get_merged(scope, locale, ttl)?;
        render_from(&merged, key, args, locale)
    }

    fn translate_static(&self, locale: &str, key: &str, args: &[(&str, Arg)]) -> String {
        let statics = static_catalogs();
        for (_, domain) in statics.by_locale.get(locale).into_iter().flatten() {
            if let Some(segments) = domain.patterns.get(key) {
                return render_with_policy(segments, args, locale, key);
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
) -> Option<String> {
    let segments = merged.patterns.get(key)?;
    Some(render_with_policy(segments, args, locale, key))
}

fn render_with_policy(
    segments: &[Segment],
    args: &[(&str, Arg)],
    locale: &str,
    key: &str,
) -> String {
    let policy = crate::domain::FormattingPolicy::for_locale(
        Locale::parse(locale).unwrap_or_default(),
        locale_timezone(locale),
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
        // Walk the fallback chain: most specific locale first, ending in
        // the source locale (en).
        let mut tried: Vec<&str> = Vec::new();
        let chain = fallback_chain(locale);
        for candidate in &chain {
            if tried.contains(&candidate.as_str()) {
                continue;
            }
            tried.push(candidate.as_str());
            if let Some(rendered) = self.translate_cached(scope, candidate, key, args) {
                return rendered;
            }
        }
        self.cache.record_miss();
        // Static fallback (never touches the DB, never empty).
        for candidate in &chain {
            let rendered = self.translate_static(candidate, key, args);
            if !rendered.is_empty() && rendered != key {
                return rendered;
            }
        }
        self.translate_static("en", key, args)
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
    fn title(&self, type_slug: &str, locale: &str) -> Option<String> {
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
        let rendered = self.translate("platform", locale, key.key, &[]);
        (rendered != key.key && !rendered.is_empty()).then_some(rendered)
    }
}
