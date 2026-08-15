use anyhow::{Context, Result};
use prometheus::{Encoder, Registry, TextEncoder};

use crate::TelemetryConfig;

/// Prometheus registry backed by an OpenTelemetry meter provider.
///
/// The registry is exposed over HTTP at `/metrics`; the meter provider feeds
/// the same instruments into the OpenTelemetry metrics pipeline (OTLP export
/// when enabled).
pub struct MetricsRegistry {
    registry: Registry,
}

impl MetricsRegistry {
    /// Registry for tests and tools: no OTel pipeline attached.
    pub fn prometheus_only() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            registry: Registry::new(),
        })
    }

    /// Encode all registered metrics in the Prometheus text exposition format.
    pub fn render(&self) -> Result<String> {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder
            .encode(&metric_families, &mut buffer)
            .context("failed to encode Prometheus metrics")?;
        String::from_utf8(buffer).context("Prometheus output was not valid UTF-8")
    }

    pub fn prometheus_registry(&self) -> &Registry {
        &self.registry
    }
}

/// Build a Prometheus registry + exporter reader for `service_name`.
pub fn instrument_registry(
    config: &TelemetryConfig,
) -> (
    std::sync::Arc<MetricsRegistry>,
    opentelemetry_prometheus::PrometheusExporter,
) {
    let registry = Registry::new();

    // Process-level USE metrics (CPU, memory, FDs) via the prometheus client.
    // The prometheus crate only supports process collection on Linux; on
    // other platforms container-level metrics come from the orchestrator.
    #[cfg(target_os = "linux")]
    {
        let process_collector = prometheus::process_collector::ProcessCollector::for_self();
        registry.register(Box::new(process_collector)).ok();
    }

    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()
        .expect("prometheus exporter construction is infallible");

    let _service_name = &config.service_name; // attributes are attached by the provider resource
    (std::sync::Arc::new(MetricsRegistry { registry }), exporter)
}

#[cfg(test)]
fn test_registry() -> std::sync::Arc<MetricsRegistry> {
    std::sync::Arc::new(MetricsRegistry {
        registry: Registry::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_metrics_even_when_empty() {
        let registry = test_registry();
        let output = registry.render().unwrap();
        assert!(output.is_empty() || !output.contains("sanvi"));
    }

    #[test]
    fn registers_a_counter_and_renders_it() {
        let registry = test_registry();
        let counter = prometheus::CounterVec::new(
            prometheus::Opts::new("sanvi_test_counter", "test counter"),
            &["label"],
        )
        .unwrap();
        registry
            .prometheus_registry()
            .register(Box::new(counter.clone()))
            .unwrap();
        counter.with_label_values(&["a"]).inc();

        let output = registry.render().unwrap();
        assert!(output.contains("sanvi_test_counter"));
        assert!(output.contains("label=\"a\""));
    }
}
