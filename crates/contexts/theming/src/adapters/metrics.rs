//! Theming metrics:
//! - `theme_resolution_duration_seconds` — p99 budget for the resolution
//!   path (alert on it, per the plan).
//! - `theme_cache_hit_total` / `theme_cache_miss_total` — the ratio drives
//!   `theme_cache_hit_ratio` dashboards.
//! - `theme_publish_total{result}` — publish outcomes.
//! - `theme_validation_failures_total{reason}` — the install pipeline's
//!   rejections, by reason.
//! - `custom_css_size_bytes` — accepted custom CSS sizes.

use prometheus::{Histogram, IntCounter, IntCounterVec, Registry};

#[derive(Clone)]
pub struct ThemingMetrics {
    pub resolution_duration: Histogram,
    pub cache_hits: IntCounter,
    pub cache_misses: IntCounter,
    pub publish_total: IntCounterVec,
    pub validation_failures: IntCounterVec,
    pub custom_css_size: Histogram,
}

impl ThemingMetrics {
    pub fn new(registry: &Registry) -> Self {
        let resolution_duration = Histogram::with_opts(prometheus::HistogramOpts::new(
            "theme_resolution_duration_seconds",
            "Duration of resolved-theme production (cache check + merge + emit)",
        ))
        .expect("theme_resolution_duration_seconds construction is infallible");
        let cache_hits = IntCounter::new(
            "theme_cache_hit_total",
            "Resolved-theme cache lookups that hit",
        )
        .expect("theme_cache_hit_total construction is infallible");
        let cache_misses = IntCounter::new(
            "theme_cache_miss_total",
            "Resolved-theme cache lookups that missed",
        )
        .expect("theme_cache_miss_total construction is infallible");
        let publish_total = IntCounterVec::new(
            prometheus::Opts::new("theme_publish_total", "Theme publishes by result"),
            &["result"],
        )
        .expect("theme_publish_total construction is infallible");
        let validation_failures = IntCounterVec::new(
            prometheus::Opts::new(
                "theme_validation_failures_total",
                "Theme install/update rejections by reason",
            ),
            &["reason"],
        )
        .expect("theme_validation_failures_total construction is infallible");
        let custom_css_size = Histogram::with_opts(prometheus::HistogramOpts::new(
            "custom_css_size_bytes",
            "Accepted custom CSS sizes in bytes",
        ))
        .expect("custom_css_size_bytes construction is infallible");

        let _ = registry.register(Box::new(resolution_duration.clone()));
        let _ = registry.register(Box::new(cache_hits.clone()));
        let _ = registry.register(Box::new(cache_misses.clone()));
        let _ = registry.register(Box::new(publish_total.clone()));
        let _ = registry.register(Box::new(validation_failures.clone()));
        let _ = registry.register(Box::new(custom_css_size.clone()));

        Self {
            resolution_duration,
            cache_hits,
            cache_misses,
            publish_total,
            validation_failures,
            custom_css_size,
        }
    }
}
