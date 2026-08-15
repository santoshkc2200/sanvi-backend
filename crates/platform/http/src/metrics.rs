use std::time::Duration;

use opentelemetry::{KeyValue, metrics::Histogram};

/// RED-style HTTP instruments, created once per process from the global meter.
///
/// Labels: `method`, `route` (template), `context` (bounded context prefix),
/// `status`.
#[derive(Clone)]
pub struct HttpInstruments {
    requests: opentelemetry::metrics::Counter<u64>,
    duration: Histogram<f64>,
    errors: opentelemetry::metrics::Counter<u64>,
}

impl HttpInstruments {
    pub fn build() -> Self {
        let meter = opentelemetry::global::meter("sanvi.http");
        Self {
            requests: meter
                .u64_counter("http.server.requests")
                .with_description("Number of HTTP requests handled")
                .build(),
            duration: meter
                .f64_histogram("http.server.duration")
                .with_unit("s")
                .with_description("HTTP request duration in seconds")
                .build(),
            errors: meter
                .u64_counter("http.server.errors")
                .with_description("Number of HTTP requests that failed (5xx)")
                .build(),
        }
    }

    /// Process-wide instance for middleware that cannot carry state.
    pub fn process() -> &'static Self {
        static INSTRUMENTS: std::sync::OnceLock<HttpInstruments> = std::sync::OnceLock::new();
        INSTRUMENTS.get_or_init(HttpInstruments::build)
    }

    pub fn record(
        &self,
        method: &str,
        route: &str,
        context: &str,
        status: u16,
        duration: Duration,
    ) {
        let labels = [
            KeyValue::new("method", method.to_string()),
            KeyValue::new("route", route.to_string()),
            KeyValue::new("context", context.to_string()),
            KeyValue::new("status", i64::from(status)),
        ];
        self.requests.add(1, &labels);
        self.duration.record(duration.as_secs_f64(), &labels);
        if status >= 500 {
            self.errors.add(1, &labels);
        }
    }
}

/// Derive the bounded-context label from a request path:
/// `/api/v1/billing/invoices` → `billing`; health endpoints → `system`.
pub fn context_from_path(path: &str) -> &str {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["api", "v1", context, ..] => context,
        ["healthz" | "readyz" | "metrics", ..] => "system",
        _ => "platform",
    }
}

/// Tenant resolution instruments: which source resolved the tenant, and how
/// long resolution took.
#[derive(Clone)]
pub struct TenantResolutionInstruments {
    duration: Histogram<f64>,
    source: opentelemetry::metrics::Counter<u64>,
}

impl TenantResolutionInstruments {
    pub fn build() -> Self {
        let meter = opentelemetry::global::meter("sanvi.tenancy");
        Self {
            duration: meter
                .f64_histogram("tenant_resolution_duration")
                .with_unit("s")
                .with_description("Tenant resolution duration in seconds")
                .build(),
            source: meter
                .u64_counter("tenant_resolution_source_total")
                .with_description("Tenant resolutions by source")
                .build(),
        }
    }

    /// Process-wide instance for the middleware (constructed once).
    pub fn process() -> &'static Self {
        static INSTRUMENTS: std::sync::OnceLock<TenantResolutionInstruments> =
            std::sync::OnceLock::new();
        INSTRUMENTS.get_or_init(TenantResolutionInstruments::build)
    }

    pub fn record(&self, duration: Duration, source: String) {
        self.duration.record(duration.as_secs_f64(), &[]);
        self.source.add(1, &[KeyValue::new("source", source)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_is_derived_from_path() {
        assert_eq!(context_from_path("/api/v1/billing/invoices"), "billing");
        assert_eq!(context_from_path("/api/v1/system/version"), "system");
        assert_eq!(context_from_path("/healthz"), "system");
        assert_eq!(context_from_path("/readyz"), "system");
        assert_eq!(context_from_path("/unknown"), "platform");
    }
}
