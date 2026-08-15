//! Prometheus instruments for the async backbone.
//!
//! Collectors are constructed independently of any registry: the worker
//! registers them on the process registry (`/metrics`), while integration
//! tests can register them on throwaway registries.

use prometheus::{Histogram, HistogramOpts, IntCounter, IntGauge, Opts, Registry, Result};

/// Outbox dispatcher instruments: pending work, publish latency, dead letters.
#[derive(Debug, Clone)]
pub struct OutboxMetrics {
    /// Unpublished rows waiting in `platform.outbox`.
    pub pending: IntGauge,
    /// Stream publish round-trip latency (seconds).
    pub publish_latency: Histogram,
    /// Events abandoned to `platform.outbox_dlq`.
    pub dead_lettered: IntCounter,
}

impl OutboxMetrics {
    pub fn new() -> Self {
        let pending = IntGauge::with_opts(Opts::new(
            "outbox_pending_gauge",
            "Number of outbox rows awaiting publication",
        ))
        .expect("metric definition is constant");
        let publish_latency = Histogram::with_opts(HistogramOpts::new(
            "outbox_publish_latency",
            "Seconds taken to publish one event to the stream",
        ))
        .expect("metric definition is constant");
        let dead_lettered = IntCounter::with_opts(Opts::new(
            "outbox_dead_lettered_total",
            "Events moved to the dead-letter table after exhausting retries",
        ))
        .expect("metric definition is constant");
        Self {
            pending,
            publish_latency,
            dead_lettered,
        }
    }

    /// Register every collector on `registry` (idempotent per registry).
    pub fn register(&self, registry: &Registry) -> Result<()> {
        registry.register(Box::new(self.pending.clone()))?;
        registry.register(Box::new(self.publish_latency.clone()))?;
        registry.register(Box::new(self.dead_lettered.clone()))?;
        Ok(())
    }
}

impl Default for OutboxMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Stream consumer instruments: per-consumer-group delivery lag and poison
/// messages.
#[derive(Debug, Clone)]
pub struct ConsumerMetrics {
    /// Delivery lag per consumer group: messages pending in the stream not
    /// yet processed by this group.
    pub lag: IntGauge,
    /// Handler failures forwarded to the dead-letter stream.
    pub dead_lettered: IntCounter,
}

impl ConsumerMetrics {
    pub fn new(group: &str) -> Self {
        let lag = IntGauge::with_opts(
            Opts::new(
                "stream_consumer_lag",
                "Messages pending for a consumer group",
            )
            .const_label("group", group),
        )
        .expect("metric definition is constant");
        let dead_lettered = IntCounter::with_opts(
            Opts::new(
                "stream_consumer_dead_lettered_total",
                "Messages a consumer failed to process",
            )
            .const_label("group", group),
        )
        .expect("metric definition is constant");
        Self { lag, dead_lettered }
    }

    /// Register every collector on `registry` (idempotent per registry).
    pub fn register(&self, registry: &Registry) -> Result<()> {
        registry.register(Box::new(self.lag.clone()))?;
        registry.register(Box::new(self.dead_lettered.clone()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_on_a_throwaway_registry() {
        let registry = Registry::new();
        let outbox = OutboxMetrics::new();
        outbox.register(&registry).unwrap();
        let consumer = ConsumerMetrics::new("tenancy");
        consumer.register(&registry).unwrap();

        outbox.pending.set(3);
        consumer.lag.set(2);
        assert_eq!(outbox.pending.get(), 3);
        assert_eq!(consumer.lag.get(), 2);
    }
}
