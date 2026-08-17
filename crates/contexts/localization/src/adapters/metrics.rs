//! Localization metrics:
//! - `locale_negotiated_total{locale,source}` — which locale, from where.
//! - `i18n_missing_key_total{key,locale}` — should be 0 in production;
//!   alert on any increase after a deploy.
//! - `translation_cache_hit_total` / `translation_cache_miss_total` — the
//!   ratio drives `translation_cache_hit_ratio` dashboards.

use prometheus::{Histogram, IntCounter, IntCounterVec, Registry};

#[derive(Clone)]
pub struct LocalizationMetrics {
    pub negotiated: IntCounterVec,
    pub missing_key: IntCounterVec,
    pub cache_hits: IntCounter,
    pub cache_misses: IntCounter,
    pub resolution_duration: Histogram,
}

impl LocalizationMetrics {
    pub fn new(registry: &Registry) -> Self {
        let negotiated = IntCounterVec::new(
            prometheus::Opts::new(
                "locale_negotiated_total",
                "Negotiated locales, labelled by locale and source",
            ),
            &["locale", "source"],
        )
        .expect("locale_negotiated_total construction is infallible");
        let missing_key = IntCounterVec::new(
            prometheus::Opts::new(
                "i18n_missing_key_total",
                "Message lookups that fell back because the key was missing, per key and locale",
            ),
            &["key", "locale"],
        )
        .expect("i18n_missing_key_total construction is infallible");
        let cache_hits = IntCounter::new(
            "translation_cache_hit_total",
            "Translation cache lookups that hit",
        )
        .expect("translation_cache_hit_total construction is infallible");
        let cache_misses = IntCounter::new(
            "translation_cache_miss_total",
            "Translation cache lookups that missed",
        )
        .expect("translation_cache_miss_total construction is infallible");
        let resolution_duration = Histogram::with_opts(prometheus::HistogramOpts::new(
            "locale_resolution_duration_seconds",
            "Duration of locale resolution (negotiation + cache warming)",
        ))
        .expect("locale_resolution_duration_seconds construction is infallible");

        let _ = registry.register(Box::new(negotiated.clone()));
        let _ = registry.register(Box::new(missing_key.clone()));
        let _ = registry.register(Box::new(cache_hits.clone()));
        let _ = registry.register(Box::new(cache_misses.clone()));
        let _ = registry.register(Box::new(resolution_duration.clone()));

        Self {
            negotiated,
            missing_key,
            cache_hits,
            cache_misses,
            resolution_duration,
        }
    }
}
