//! Sanvi telemetry: structured logging, OpenTelemetry traces/metrics and a
//! Prometheus scrape endpoint, wired from one typed configuration.
//!
//! Layout:
//! - [`init`] builds the global `tracing` subscriber (JSON or human-readable),
//!   the OTLP tracer exporter, the Prometheus metrics pipeline, and returns a
//!   [`TelemetryGuard`] that shuts everything down cleanly on drop.
//! - The OTLP receiver is OpenObserve (gRPC :5081), which authenticates every
//!   export; see [`OtlpConfig`] for the metadata that carries.
//! - [`Redacted`] is the approved way to log sensitive values: it always
//!   renders as `<redacted>`, no matter which `fmt`/`Debug` implementation the
//!   wrapped type has.

mod config;
mod metrics;
mod redaction;
mod sampling;

use std::sync::Arc;

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig, WithTonicConfig};
use opentelemetry_sdk::{
    Resource,
    metrics::{PeriodicReader, SdkMeterProvider},
    trace::SdkTracerProvider,
};
use tonic::metadata::{Ascii, MetadataMap, MetadataValue};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

pub use config::{OtlpConfig, SamplingConfig, TelemetryConfig, TelemetryLogFormat};
pub use metrics::{MetricsRegistry, instrument_registry};
pub use redaction::Redacted;
pub use sampling::TailSamplingProcessor;

/// Handle returned by [`init`]; flushes and shuts down providers when dropped.
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl TelemetryGuard {
    /// Flush pending spans and shut down the OTel providers.
    ///
    /// Idempotent: safe to call right before process exit; `Drop` covers it.
    pub fn shutdown(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.meter_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn build_resource(config: &TelemetryConfig) -> Resource {
    use opentelemetry_semantic_conventions::resource::{DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_NAME};
    Resource::builder()
        .with_attributes([
            opentelemetry::KeyValue::new(SERVICE_NAME, config.service_name.clone()),
            opentelemetry::KeyValue::new(DEPLOYMENT_ENVIRONMENT_NAME, config.environment.clone()),
        ])
        .build()
}

/// gRPC metadata OpenObserve requires on every OTLP export: which org and
/// stream the data belongs to, and who is sending it.
fn otlp_metadata(config: &OtlpConfig) -> Result<MetadataMap> {
    let mut metadata = MetadataMap::with_capacity(3);
    metadata.insert(
        "organization",
        config
            .organization
            .parse()
            .context("telemetry.otlp.organization is not a valid gRPC metadata value")?,
    );
    metadata.insert(
        "stream-name",
        config
            .stream
            .parse()
            .context("telemetry.otlp.stream is not a valid gRPC metadata value")?,
    );
    // Deliberately not `context`-ed with the value: this is the credential.
    let authorization: MetadataValue<Ascii> = format!("Basic {}", config.auth_token)
        .parse()
        .context("telemetry.otlp.auth_token is not a valid gRPC metadata value")?;
    metadata.insert("authorization", authorization);
    Ok(metadata)
}

pub(crate) fn build_otlp_tracer(config: &TelemetryConfig) -> Result<Option<SdkTracerProvider>> {
    if !config.otlp.enabled {
        return Ok(None);
    }
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(config.otlp.endpoint.clone())
        .with_metadata(otlp_metadata(&config.otlp)?)
        .build()
        .context("failed to build OTLP span exporter")?;
    // Head sampling always decides at span start; it is the fallback when
    // the collector is unavailable, and the per-trace baseline the tail
    // processor refines when it is on.
    let head_ratio = config.sampling.head_ratio;
    let mut builder = SdkTracerProvider::builder()
        .with_sampler(opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
            opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(head_ratio),
        )))
        .with_resource(build_resource(config));
    if config.sampling.tail_sampling {
        builder = builder.with_span_processor(sampling::TailSamplingProcessor::new(
            opentelemetry_sdk::trace::BatchSpanProcessor::builder(exporter).build(),
            std::time::Duration::from_millis(config.sampling.latency_threshold_ms),
            head_ratio,
        ));
    } else {
        builder = builder.with_batch_exporter(exporter);
    }
    Ok(Some(builder.build()))
}

fn build_otlp_meter_reader(
    config: &TelemetryConfig,
) -> Result<Option<PeriodicReader<MetricExporter>>> {
    if !config.otlp.enabled {
        return Ok(None);
    }
    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(config.otlp.endpoint.clone())
        .with_metadata(otlp_metadata(&config.otlp)?)
        .build()
        .context("failed to build OTLP metric exporter")?;
    Ok(Some(PeriodicReader::builder(exporter).build()))
}

fn log_filter(config: &TelemetryConfig) -> EnvFilter {
    let default_level = config
        .default_level
        .parse::<LevelFilter>()
        .unwrap_or(LevelFilter::INFO);
    EnvFilter::builder()
        .with_default_directive(default_level.into())
        .from_env_lossy()
}

/// Initialise telemetry and return the metrics registry plus a shutdown guard.
pub fn init(config: TelemetryConfig) -> Result<(Arc<MetricsRegistry>, TelemetryGuard)> {
    config
        .validate()
        .context("invalid telemetry configuration")?;

    // The Prometheus exporter is always on: it powers /metrics locally and in
    // every environment, independent of OTLP export.
    let (registry, prometheus_exporter) = instrument_registry(&config);

    let otlp_tracer = build_otlp_tracer(&config)?;
    let otlp_meter_reader = build_otlp_meter_reader(&config)?;

    // One meter provider feeds every pipeline: Prometheus scraping plus the
    // optional OTLP push exporter.
    let meter_provider = {
        let mut builder = SdkMeterProvider::builder()
            .with_resource(build_resource(&config))
            .with_reader(prometheus_exporter);
        if let Some(reader) = otlp_meter_reader {
            builder = builder.with_reader(reader);
        }
        builder.build()
    };
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    if let Some(provider) = &otlp_tracer {
        opentelemetry::global::set_tracer_provider(provider.clone());
    }

    // W3C TraceContext is the correlation contract (TASK-020): inbound
    // `traceparent` headers join the caller's trace at the root span, and
    // the error bodies / response headers name the same trace. Registered
    // unconditionally so local runs extract the same way, into a noop
    // propagator's empty context when OTLP is off.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let fmt_layer = match config.log_format {
        TelemetryLogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .boxed(),
        TelemetryLogFormat::Text => tracing_subscriber::fmt::layer().with_target(false).boxed(),
    };

    let subscriber = tracing_subscriber::registry()
        .with(log_filter(&config))
        .with(fmt_layer);

    // The OpenTelemetry layer is always present so the subscriber type stays
    // uniform; without OTLP it binds to a no-op tracer and filters to OFF.
    let otel_layer = match &otlp_tracer {
        Some(provider) => tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("sanvi"))
            .with_filter(LevelFilter::INFO)
            .boxed(),
        None => tracing_opentelemetry::layer()
            .with_tracer(opentelemetry::trace::noop::NoopTracer::new())
            .with_filter(LevelFilter::OFF)
            .boxed(),
    };

    let subscriber = subscriber
        .with(otel_layer)
        .with(tracing_opentelemetry::MetricsLayer::new(
            meter_provider.clone(),
        ))
        .try_init();

    if let Err(err) = subscriber {
        anyhow::bail!("failed to initialise tracing subscriber: {err}");
    }

    let guard = TelemetryGuard {
        tracer_provider: otlp_tracer,
        meter_provider: Some(meter_provider),
    };

    Ok((registry, guard))
}
