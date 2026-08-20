//! Prometheus metrics for the payments context.
//!
//! 09.0 wires the catalog path so observability arrives with the first
//! route, not retrofitted in 09.7. The money-path metrics from the parent
//! plan (`checkout_created_total`, `payment_succeeded_total`, …) register
//! with the slices that first emit them.

use prometheus::{IntCounter, IntCounterVec, Opts, Registry};

#[derive(Clone)]
pub struct PaymentsMetrics {
    /// Catalog reads by outcome — the one live path in 09.0.
    pub catalog_requests: IntCounterVec,
    /// Entitlement gate denials on payments routes (the upgrade-prompt
    /// funnel; worth its own series from day one).
    pub entitlement_denied: IntCounter,
}

impl PaymentsMetrics {
    pub fn new(registry: &Registry) -> Self {
        let catalog_requests = IntCounterVec::new(
            Opts::new(
                "payments_catalog_requests_total",
                "Provider-catalog reads by outcome",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let entitlement_denied = IntCounter::new(
            "payments_entitlement_denied_total",
            "Payments routes denied by the payments.stripe_connect entitlement gate",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(catalog_requests.clone()));
        let _ = registry.register(Box::new(entitlement_denied.clone()));
        Self {
            catalog_requests,
            entitlement_denied,
        }
    }
}
