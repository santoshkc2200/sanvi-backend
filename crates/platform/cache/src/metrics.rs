//! Hit-ratio instruments per cache context (TASK-021 11.2g: "hit ratios per
//! cache" — a cache nobody measures is a guess with a TTL). Adapters feed
//! the counters; the registry exposes them next to the pool and outbox
//! gauges.
//!
//! `context` labels the logical cache (`theming.resolved`, `access.entitlements`,
//! …), not the key: the label set stays bounded regardless of tenant count.

use prometheus::{IntCounterVec, Opts, Registry};

/// Per-context cache counters registered on the process Prometheus registry.
#[derive(Clone)]
pub struct CacheMetrics {
    hits: IntCounterVec,
    misses: IntCounterVec,
    /// Misses that joined an in-flight load instead of loading (the
    /// stampede guard's saving, sampled per context).
    joins: IntCounterVec,
}

impl CacheMetrics {
    pub fn new(registry: &Registry) -> Self {
        let hits = IntCounterVec::new(
            Opts::new("cache_hits_total", "Cache reads served without a load"),
            &["context"],
        )
        .expect("counter construction is infallible");
        let misses = IntCounterVec::new(
            Opts::new(
                "cache_misses_total",
                "Cache reads that fell through to a load",
            ),
            &["context"],
        )
        .expect("counter construction is infallible");
        let joins = IntCounterVec::new(
            Opts::new(
                "cache_stampede_joins_total",
                "Concurrent misses that joined an in-flight load for the same key",
            ),
            &["context"],
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(hits.clone()));
        let _ = registry.register(Box::new(misses.clone()));
        let _ = registry.register(Box::new(joins.clone()));
        Self {
            hits,
            misses,
            joins,
        }
    }

    /// Unregistered counters for tests and single-process deployments —
    /// counting without a registry.
    pub fn noop() -> Self {
        let build = |name: &str, help: &str| {
            IntCounterVec::new(Opts::new(name, help), &["context"])
                .expect("counter construction is infallible")
        };
        Self {
            hits: build("cache_hits_total", "Cache reads served without a load"),
            misses: build(
                "cache_misses_total",
                "Cache reads that fell through to a load",
            ),
            joins: build(
                "cache_stampede_joins_total",
                "Concurrent misses that joined an in-flight load for the same key",
            ),
        }
    }

    pub fn hit(&self, context: &str) {
        self.hits.with_label_values(&[context]).inc();
    }

    pub fn miss(&self, context: &str) {
        self.misses.with_label_values(&[context]).inc();
    }

    pub fn join(&self, context: &str) {
        self.joins.with_label_values(&[context]).inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_render_on_the_registry_with_the_context_label() {
        let registry = Registry::new();
        let metrics = CacheMetrics::new(&registry);
        metrics.hit("access.entitlements");
        metrics.miss("access.entitlements");
        metrics.miss("access.entitlements");
        metrics.join("access.entitlements");
        let rendered = prometheus::TextEncoder::new()
            .encode_to_string(&registry.gather())
            .expect("text encoding is infallible");
        assert!(rendered.contains(r#"cache_hits_total{context="access.entitlements"} 1"#));
        assert!(rendered.contains(r#"cache_misses_total{context="access.entitlements"} 2"#));
        assert!(
            rendered.contains(r#"cache_stampede_joins_total{context="access.entitlements"} 1"#)
        );
    }

    #[test]
    fn noop_counters_count_without_a_registry() {
        let metrics = CacheMetrics::noop();
        metrics.hit("test");
        metrics.join("test");
    }
}
