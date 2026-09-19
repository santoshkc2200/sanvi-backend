//! Tail-based span sampling (TASK-020 step 5), behind the
//! `telemetry.sampling.tail_sampling` flag (the task's
//! `platform.tail_sampling`).
//!
//! The processor sits between the tracer provider and the batch exporter
//! and decides, at span end, whether a span is worth exporting:
//!
//! - spans that ended in an error are always kept — they are the reason
//!   traces exist;
//! - spans slower than the configured latency threshold are always kept —
//!   the latency outliers the budgets gate on;
//! - everything else is decided **per trace**: the first ordinary span of a
//!   trace to end applies the head-ratio coin, and the answer is sticky, so
//!   a trace is exported whole or dropped whole rather than arriving in
//!   OpenObserve as confetti.
//!
//! When the flag is on this processor is the *sole* decider:
//! `build_otlp_tracer` pairs it with `Sampler::AlwaysOn`, because a head
//! sampler drops non-sampled spans before any processor sees them — under
//! `ParentBased(TraceIdRatioBased)` the keep-errors promise would only hold
//! inside head-sampled traces, and ordinary spans would draw the ratio coin
//! twice. The provider-level test `tail_sampling_sees_spans_the_head_coin_
//! would_drop` pins that seam.
//!
//! With the flag off the processor is not installed at all and the
//! provider's `ParentBased(TraceIdRatioBased(head_ratio))` is the whole
//! policy. Either way, with the collector unavailable the batch exporter
//! fails asynchronously — sampling decisions never block or fail a request,
//! which is the invariant the init tests assert.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use opentelemetry::trace::Status;
use opentelemetry::trace::TraceId;
use opentelemetry_sdk::trace::SpanData;
use opentelemetry_sdk::trace::SpanProcessor;

/// Per-trace sticky decisions live in a bounded map: a lab-shaped process
/// sees thousands of short traces per second, and unbounded growth here
/// would be a memory leak wearing a sampling costume. At the cap one
/// arbitrary entry is evicted per new decision — at that size the evicted
/// entry is almost certainly a long-closed trace, and every other in-flight
/// trace keeps its stickiness. (A wholesale clear was rejected: it would
/// turn cap pressure into exactly the partial-trace confetti the sticky
/// decision exists to prevent.)
const DECISION_MAP_CAP: usize = 65_536;

/// Reserve room for `incoming`: at the cap, evict one arbitrary entry.
fn make_room(decisions: &mut HashMap<TraceId, bool>, incoming: TraceId) {
    if decisions.len() < DECISION_MAP_CAP || decisions.contains_key(&incoming) {
        return;
    }
    if let Some(evicted) = decisions.keys().next().copied() {
        decisions.remove(&evicted);
    }
}

#[derive(Debug)]
pub struct TailSamplingProcessor<P> {
    inner: P,
    latency_threshold: Duration,
    head_ratio: f64,
    decisions: Mutex<HashMap<TraceId, bool>>,
}

impl<P: SpanProcessor> TailSamplingProcessor<P> {
    /// Wrap `inner` (the batch exporter's processor): spans ending in error
    /// or slower than `latency_threshold` are always exported; other spans
    /// follow the first per-trace decision, drawn with probability
    /// `head_ratio`.
    pub fn new(inner: P, latency_threshold: Duration, head_ratio: f64) -> Self {
        Self {
            inner,
            latency_threshold,
            head_ratio: head_ratio.clamp(0.0, 1.0),
            decisions: Mutex::new(HashMap::new()),
        }
    }

    fn keep(&self, span: &SpanData) -> bool {
        let duration = span
            .end_time
            .duration_since(span.start_time)
            .unwrap_or_default();
        let notable =
            matches!(span.status, Status::Error { .. }) || duration >= self.latency_threshold;
        let trace_id = span.span_context.trace_id();
        if notable {
            // Errors and latency outliers make their whole trace worth
            // keeping: record the decision so later ordinary spans of the
            // same trace follow it into OpenObserve.
            if let Ok(mut decisions) = self.decisions.lock() {
                make_room(&mut decisions, trace_id);
                decisions.insert(trace_id, true);
            }
            true
        } else {
            match self.decisions.lock() {
                // Consult the trace's existing decision, or draw the head
                // coin for it.
                Ok(mut decisions) => {
                    make_room(&mut decisions, trace_id);
                    *decisions
                        .entry(trace_id)
                        .or_insert_with(rand_keep(self.head_ratio))
                }
                Err(_) => false,
            }
        }
    }
}

/// SplitMix64 state: a self-seeding PRNG is plenty for a sampling coin and
/// adds no dependency; the same generator the seed-scale data uses.
static SAMPLER_STATE: AtomicU64 = AtomicU64::new(0x9E3779B97F4A7C15);

fn next_random() -> u64 {
    let mut z = SAMPLER_STATE.fetch_add(0x9E3779B97F4A7C15, Ordering::Relaxed);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// A coin with win probability `ratio`. Returned as a closure so
/// `or_insert_with` pays for the draw only when a decision is missing.
fn rand_keep(ratio: f64) -> impl Fn() -> bool {
    move || ratio >= 1.0 || (next_random() as f64 / u64::MAX as f64) < ratio
}

impl<P: SpanProcessor> SpanProcessor for TailSamplingProcessor<P> {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
        self.inner.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        if self.keep(&span) {
            self.inner.on_end(span);
        }
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.force_flush()
    }

    fn shutdown_with_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::SpanContext;
    use opentelemetry::trace::SpanKind;
    use opentelemetry::trace::Status as OtStatus;
    use opentelemetry::trace::TraceFlags;
    use opentelemetry::trace::TraceState;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::trace::SpanEvents;
    use opentelemetry_sdk::trace::SpanLinks;
    use std::borrow::Cow;
    use std::sync::atomic::AtomicUsize;
    use std::time::SystemTime;

    /// An inner processor that counts what it was handed.
    #[derive(Debug, Default)]
    struct CountingProcessor {
        ended: AtomicUsize,
    }

    impl CountingProcessor {
        fn ended(&self) -> usize {
            self.ended.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl SpanProcessor for CountingProcessor {
        fn on_start(
            &self,
            _span: &mut opentelemetry_sdk::trace::Span,
            _cx: &opentelemetry::Context,
        ) {
        }

        fn on_end(&self, _span: SpanData) {
            self.ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(
            &self,
            _timeout: std::time::Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn set_resource(&mut self, _resource: &Resource) {}
    }

    fn span(trace: u128, duration: Duration, status: Status) -> SpanData {
        SpanData {
            span_context: SpanContext::new(
                TraceId::from_bytes(trace.to_be_bytes()),
                opentelemetry::trace::SpanId::from_bytes(1_u64.to_be_bytes()),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id: opentelemetry::trace::SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Internal,
            name: Cow::Borrowed("probe"),
            start_time: SystemTime::UNIX_EPOCH,
            end_time: SystemTime::UNIX_EPOCH + duration,
            attributes: Vec::new(),
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status,
            instrumentation_scope: opentelemetry::InstrumentationScope::builder("test").build(),
        }
    }

    #[test]
    fn error_spans_are_always_kept() {
        let inner = CountingProcessor::default();
        let processor = TailSamplingProcessor::new(inner, Duration::from_secs(1), 0.0);
        processor.on_end(span(1, Duration::from_millis(1), Status::error("boom")));
        assert_eq!(processor.inner.ended(), 1);
    }

    #[test]
    fn slow_spans_are_always_kept() {
        let inner = CountingProcessor::default();
        let processor = TailSamplingProcessor::new(inner, Duration::from_millis(100), 0.0);
        processor.on_end(span(2, Duration::from_millis(150), OtStatus::Unset));
        assert_eq!(processor.inner.ended(), 1);
    }

    #[test]
    fn ordinary_spans_follow_the_head_ratio() {
        // ratio 0: every ordinary span of a fresh trace is dropped.
        let inner = CountingProcessor::default();
        let processor = TailSamplingProcessor::new(inner, Duration::from_millis(100), 0.0);
        processor.on_end(span(3, Duration::from_millis(1), OtStatus::Unset));
        processor.on_end(span(4, Duration::from_millis(1), OtStatus::Unset));
        assert_eq!(processor.inner.ended(), 0);

        // ratio 1: every ordinary span is kept.
        let inner = CountingProcessor::default();
        let processor = TailSamplingProcessor::new(inner, Duration::from_millis(100), 1.0);
        processor.on_end(span(5, Duration::from_millis(1), OtStatus::Unset));
        assert_eq!(processor.inner.ended(), 1);
    }

    #[test]
    fn the_per_trace_decision_is_sticky() {
        let inner = CountingProcessor::default();
        // ratio 0 — the only way a trace is kept is an error or slow span.
        let processor = TailSamplingProcessor::new(inner, Duration::from_millis(100), 0.0);
        processor.on_end(span(6, Duration::from_millis(1), Status::error("boom")));
        // Sibling spans of the same trace stay together with the error.
        processor.on_end(span(6, Duration::from_millis(1), OtStatus::Unset));
        processor.on_end(span(6, Duration::from_millis(1), OtStatus::Unset));
        // A different trace has no decision of its own and is dropped.
        processor.on_end(span(7, Duration::from_millis(1), OtStatus::Unset));
        assert_eq!(processor.inner.ended(), 3);
    }

    /// Cap pressure evicts one arbitrary decision per new trace, not the
    /// map: the bound holds and no wholesale wipe occurs (a wholesale clear
    /// would re-coin every in-flight trace's remaining spans into confetti).
    #[test]
    fn cap_pressure_evicts_one_entry_not_the_whole_map() {
        let inner = CountingProcessor::default();
        let processor = TailSamplingProcessor::new(inner, Duration::from_secs(1), 1.0);
        for trace in 1..=(DECISION_MAP_CAP as u128 + 1) {
            processor.on_end(span(trace, Duration::from_millis(1), OtStatus::Unset));
        }
        assert_eq!(processor.inner.ended(), DECISION_MAP_CAP + 1);
        assert_eq!(processor.decisions.lock().unwrap().len(), DECISION_MAP_CAP);
    }

    /// `force_flush`/`shutdown` delegate to the inner processor; a hanging
    /// sink is the batch exporter's problem, not the sampler's — asserted
    /// by the init test's time bound, not here.
    #[test]
    fn force_flush_and_shutdown_delegate_to_the_inner_processor() {
        let inner = CountingProcessor::default();
        let processor = TailSamplingProcessor::new(inner, Duration::from_secs(1), 1.0);
        assert!(processor.force_flush().is_ok());
        assert!(processor.shutdown().is_ok());
    }

    /// The composition seam: with tail sampling on, the provider must
    /// *record* spans the head coin would drop — the processor is the sole
    /// decider, and a head sampler would never let it see them (`on_end` is
    /// not called for dropped spans). With tail off, head ratio 0 drops at
    /// span start as before. Pointed at a dead collector; nothing is
    /// exported, only recorded.
    #[tokio::test]
    async fn tail_sampling_sees_spans_the_head_coin_would_drop() {
        use opentelemetry::trace::Span as _;
        use opentelemetry::trace::Tracer as _;
        use opentelemetry::trace::TracerProvider as _;

        let config = |tail_sampling| crate::TelemetryConfig {
            environment: "test".to_string(),
            service_name: "sanvi-test".to_string(),
            otlp: crate::OtlpConfig {
                enabled: true,
                // Nothing listens here: the exporter target is dead.
                endpoint: "http://127.0.0.1:1".to_string(),
                organization: "default".to_string(),
                stream: "default".to_string(),
                auth_token: "dGVzdDp0ZXN0".to_string(),
            },
            sampling: crate::SamplingConfig {
                head_ratio: 0.0,
                tail_sampling,
                latency_threshold_ms: 1_000,
            },
            ..crate::TelemetryConfig::default()
        };

        let provider = crate::build_otlp_tracer(&config(true))
            .expect("tracer provider builds with an unreachable collector")
            .expect("otlp is enabled");
        let kept = provider
            .tracer("composition probe")
            .start("kept-for-the-tail");
        assert!(
            kept.is_recording(),
            "tail sampling must see spans the head coin would drop"
        );

        let provider = crate::build_otlp_tracer(&config(false))
            .expect("tracer provider builds with an unreachable collector")
            .expect("otlp is enabled");
        let dropped = provider.tracer("composition probe").start("head-dropped");
        assert!(
            !dropped.is_recording(),
            "with tail off, head_ratio 0 drops at span start"
        );
    }

    /// The DoD test: with the collector unreachable, the pipeline still
    /// initialises, spans flow through the sampler into the (failing) batch
    /// exporter without blocking the caller, and shutdown completes —
    /// requests still complete. Asserted in both flag positions: tail off
    /// (sampling is exactly the head policy) and tail on (errors and slow
    /// spans are kept on top of it). The tokio runtime is required by the
    /// tonic exporter's channel, which production provides from `main`.
    #[tokio::test]
    async fn collector_unavailable_requests_still_complete() {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        for tail in [false, true] {
            let config = crate::TelemetryConfig {
                environment: "test".to_string(),
                service_name: "sanvi-test".to_string(),
                otlp: crate::OtlpConfig {
                    enabled: true,
                    // Nothing listens here: the exporter target is dead.
                    endpoint: "http://127.0.0.1:1".to_string(),
                    organization: "default".to_string(),
                    stream: "default".to_string(),
                    auth_token: "dGVzdDp0ZXN0".to_string(),
                },
                sampling: crate::SamplingConfig {
                    head_ratio: 1.0,
                    tail_sampling: tail,
                    latency_threshold_ms: 1_000,
                },
                ..crate::TelemetryConfig::default()
            };
            let provider = crate::build_otlp_tracer(&config)
                .expect("tracer provider builds with an unreachable collector")
                .expect("otlp is enabled");
            let tracer = provider.tracer("collector-down probe");
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(tracer));
            let _guard = tracing::subscriber::set_default(subscriber);

            let started = std::time::Instant::now();
            // Span emission must come back promptly — losing traces must
            // never cost requests.
            for _ in 0..16 {
                let span = tracing::info_span!("collector-down probe");
                let _enter = span.enter();
            }
            // Shutdown is bounded: with a dead collector the batch
            // exporter reports a timeout instead of hanging or panicking.
            let _ = provider.shutdown();
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "telemetry pipeline blocked for {:?} with the collector down (tail_sampling={tail})",
                started.elapsed()
            );
        }
    }
}
