//! Billing domain metrics: webhook backlog/lag, subscription status
//! distribution, dunning gauge, reconciliation drift, MRR. Registered by
//! the worker; the plan's alerting rules (unprocessed webhooks > 5 min,
//! drift > 0) read these.

use chrono::{DateTime, Utc};

#[derive(Clone)]
pub struct BillingMetrics {
    unprocessed: prometheus::IntGauge,
    lag_seconds: prometheus::Histogram,
    status_gauge: prometheus::IntGaugeVec,
    dunning_active: prometheus::IntGauge,
    drift_total: prometheus::IntCounter,
    mrr: prometheus::GaugeVec,
    dead_letter_total: prometheus::IntGauge,
}

impl BillingMetrics {
    pub fn new(registry: &prometheus::Registry) -> Self {
        let unprocessed = prometheus::IntGauge::new(
            "webhook_unprocessed_gauge",
            "Stripe webhook events stored but not yet processed",
        )
        .expect("webhook unprocessed gauge construction is infallible");
        let lag_seconds = prometheus::Histogram::with_opts(prometheus::HistogramOpts::new(
            "webhook_lag_seconds",
            "Seconds between webhook reception and processing",
        ))
        .expect("webhook lag histogram construction is infallible");
        let status_gauge = prometheus::IntGaugeVec::new(
            prometheus::Opts::new(
                "subscription_status_gauge",
                "Local subscriptions by Stripe status",
            ),
            &["status"],
        )
        .expect("subscription status gauge construction is infallible");
        let dunning_active = prometheus::IntGauge::new(
            "dunning_active_gauge",
            "Subscriptions inside the dunning grace window",
        )
        .expect("dunning gauge construction is infallible");
        let drift_total = prometheus::IntCounter::new(
            "reconciliation_drift_total",
            "Subscription drifts repaired by reconciliation",
        )
        .expect("drift counter construction is infallible");
        let mrr = prometheus::GaugeVec::new(
            prometheus::Opts::new(
                "mrr_gauge",
                "Monthly recurring revenue in minor units, normalized by interval",
            ),
            &["currency"],
        )
        .expect("mrr gauge construction is infallible");
        let dead_letter_total = prometheus::IntGauge::new(
            "webhook_dead_letter_gauge",
            "Stripe webhook events that exhausted their retry budget",
        )
        .expect("webhook dead letter gauge construction is infallible");
        let _ = registry.register(Box::new(unprocessed.clone()));
        let _ = registry.register(Box::new(lag_seconds.clone()));
        let _ = registry.register(Box::new(status_gauge.clone()));
        let _ = registry.register(Box::new(dunning_active.clone()));
        let _ = registry.register(Box::new(drift_total.clone()));
        let _ = registry.register(Box::new(mrr.clone()));
        let _ = registry.register(Box::new(dead_letter_total.clone()));
        Self {
            unprocessed,
            lag_seconds,
            status_gauge,
            dunning_active,
            drift_total,
            mrr,
            dead_letter_total,
        }
    }

    pub fn unprocessed(&self) -> &prometheus::IntGauge {
        &self.unprocessed
    }

    pub fn lag_seconds(&self) -> &prometheus::Histogram {
        &self.lag_seconds
    }

    pub fn status_gauge(&self) -> &prometheus::IntGaugeVec {
        &self.status_gauge
    }

    pub fn dunning_active(&self) -> &prometheus::IntGauge {
        &self.dunning_active
    }

    pub fn drift_total(&self) -> &prometheus::IntCounter {
        &self.drift_total
    }

    pub fn mrr(&self) -> &prometheus::GaugeVec {
        &self.mrr
    }

    pub fn dead_letter_total(&self) -> &prometheus::IntGauge {
        &self.dead_letter_total
    }

    /// Observe the lag of one processed event.
    pub fn observe_lag(&self, received_at: DateTime<Utc>, now: DateTime<Utc>) {
        self.lag_seconds
            .observe((now - received_at).num_seconds() as f64);
    }
}

/// No-op metrics for tests and non-worker processes (the API does not need
/// billing gauges; it only emits the Stripe request metrics).
impl Default for BillingMetrics {
    fn default() -> Self {
        Self::new(&prometheus::Registry::new())
    }
}
