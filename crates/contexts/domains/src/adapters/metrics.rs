//! Prometheus metrics for the domains context.

use prometheus::{Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Registry};

#[derive(Clone)]
pub struct DomainsMetrics {
    pub verification_duration: Histogram,
    pub acme_issue_total: IntCounterVec,
    pub acme_renewal_total: IntCounterVec,
    pub tls_authorize_denied_total: IntCounter,
    pub registrar_requests: IntCounterVec,
    pub health_degraded: IntGauge,
    pub status_total: IntCounterVec,
}

impl DomainsMetrics {
    pub fn new(registry: &Registry) -> Self {
        let verification_duration = Histogram::with_opts(
            HistogramOpts::new(
                "domain_verification_duration_seconds",
                "Time from claim to verified state (observed at verification)",
            )
            .buckets(vec![
                10.0, 60.0, 300.0, 1800.0, 3600.0, 21600.0, 86400.0, 259200.0,
            ]),
        )
        .expect("histogram construction is infallible");
        let acme_issue_total = IntCounterVec::new(
            prometheus::Opts::new(
                "acme_issue_total",
                "Certificate issuance attempts by outcome",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let acme_renewal_total = IntCounterVec::new(
            prometheus::Opts::new(
                "acme_renewal_total",
                "Certificate renewal attempts by outcome",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let tls_authorize_denied_total = IntCounter::new(
            "tls_authorize_denied_total",
            "TLS authorization requests denied by the gate",
        )
        .expect("counter construction is infallible");
        let registrar_requests = IntCounterVec::new(
            prometheus::Opts::new("registrar_api_requests_total", "Registrar calls by outcome"),
            &["operation", "result"],
        )
        .expect("counter vec construction is infallible");
        let health_degraded = IntGauge::new(
            "domain_health_degraded_gauge",
            "Domains currently in the degraded state",
        )
        .expect("gauge construction is infallible");
        let status_total = IntCounterVec::new(
            prometheus::Opts::new(
                "domain_status_transitions_total",
                "Domain state transitions by destination status",
            ),
            &["status"],
        )
        .expect("counter vec construction is infallible");

        let _ = registry.register(Box::new(verification_duration.clone()));
        let _ = registry.register(Box::new(acme_issue_total.clone()));
        let _ = registry.register(Box::new(acme_renewal_total.clone()));
        let _ = registry.register(Box::new(tls_authorize_denied_total.clone()));
        let _ = registry.register(Box::new(registrar_requests.clone()));
        let _ = registry.register(Box::new(health_degraded.clone()));
        let _ = registry.register(Box::new(status_total.clone()));

        Self {
            verification_duration,
            acme_issue_total,
            acme_renewal_total,
            tls_authorize_denied_total,
            registrar_requests,
            health_degraded,
            status_total,
        }
    }
}
