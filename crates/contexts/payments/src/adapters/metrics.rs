//! Prometheus metrics for the payments context.
//!
//! 09.0 wires the catalog path so observability arrives with the first
//! route, not retrofitted in 09.7. The money-path metrics from the parent
//! plan (`checkout_created_total`, `payment_succeeded_total`, …) register
//! with the slices that first emit them; 09.3 adds the Connect webhook
//! transport's lag/backlog/DLQ depth and the connection-status gauge.

use chrono::{DateTime, Utc};
use prometheus::{
    Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry,
};

#[derive(Clone)]
pub struct PaymentsMetrics {
    /// Catalog reads by outcome — the one live path in 09.0.
    pub catalog_requests: IntCounterVec,
    /// Entitlement gate denials on payments routes (the upgrade-prompt
    /// funnel; worth its own series from day one).
    pub entitlement_denied: IntCounter,
    /// Connection creates by outcome (09.2) — the onboarding funnel's
    /// entry (`connect_onboarding_started` in the plan's vocabulary).
    pub connection_creates: IntCounterVec,
    /// Account Sessions minted by outcome (09.2) — every render of the
    /// embedded onboarding component.
    pub account_sessions: IntCounterVec,
    /// Reception → processing lag for Connect webhooks (09.3); alerting
    /// watches the backlog gauge, this shows how far behind we run.
    pub webhook_lag_seconds: Histogram,
    /// Stored but unprocessed Connect events (the queue's depth).
    pub webhook_unprocessed: IntGauge,
    /// Events whose retry budget is exhausted (dead-letter depth — any
    /// value above zero pages).
    pub webhook_dead_letter: IntGauge,
    /// Dispatch outcomes per pass (`processed` / `skipped` / `failed`).
    pub webhook_events: IntCounterVec,
    /// Events for accounts we do not know — a leaked endpoint or an
    /// orphan create; any sustained rate is investigated.
    pub webhook_unknown_account: IntCounter,
    /// Live connections per status (`connection_status_gauge{status}` in
    /// the plan's vocabulary; stuck-Onboarding and Restricted alerts read
    /// it).
    pub connection_status: IntGaugeVec,
    /// Connections reaching `Active` (`connect_onboarding_completed`
    /// semantics — the funnel's exit).
    pub connection_activations: IntCounter,
    /// Reconciliation passes' work counters.
    pub reconciliation_checked: IntCounter,
    pub reconciliation_updated: IntCounter,
    /// Checkout creations by outcome (`created` / `replay`) — the top of
    /// the conversion funnel phase 10 measures downstream (09.4).
    pub checkout_creates: IntCounterVec,
    /// Payout failures observed from `payout.failed` (09.6) — each one is
    /// the tenant's money stuck at the provider; any increment pages.
    pub payout_failures: IntCounter,
    /// Tax-compliance lapses detected on tenants with automatic tax
    /// enabled (09.6) — silent under-collection risk.
    pub tax_compliance_lapsed: IntCounter,
    /// Payments recorded `succeeded` (the plan's
    /// `payment_succeeded_total`) — the top of the money funnel phase 10
    /// consumes (09.7).
    pub payment_succeeded: IntCounter,
    /// Payments that ended `failed`, by the provider's failure code
    /// (`payment_failure_total{code}`) (09.7).
    pub payment_failures: IntCounterVec,
    /// Refunds that landed `succeeded` (`refunds_issued_total`) (09.7).
    pub refunds_issued: IntCounter,
    /// Payouts confirmed arrived in the tenant's bank
    /// (`payouts_paid_total`) — the happy-side twin of
    /// [`PaymentsMetrics::payout_failures`] (09.7).
    pub payouts_paid: IntCounter,
    /// Disputes currently open (needs response / under review). The
    /// dispute runbook and the disconnect guard both care about this
    /// number staying visible (09.7).
    pub dispute_open: IntGauge,
    /// **The invariant alarm** (09.7): any platform-balance movement from
    /// a tenant transaction. Direct charges never touch the platform
    /// balance, so this should be impossible forever; a non-zero value
    /// means Sanvi has become a party to the money flow and pages an
    /// operator immediately.
    pub platform_balance_movements: IntCounter,
    /// Tenant-initiated disconnects by outcome (`ok` / the blocker code)
    /// — the settings page's exit funnel (09.7).
    pub connection_disconnects: IntCounterVec,
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
        let _ = registry.register(Box::new(catalog_requests.clone()));
        let entitlement_denied = IntCounter::new(
            "payments_entitlement_denied_total",
            "Payments routes denied by the payments.stripe_connect entitlement gate",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(entitlement_denied.clone()));
        let connection_creates = IntCounterVec::new(
            Opts::new(
                "payments_connection_creates_total",
                "Provider-connection creates by outcome (09.2 onboarding funnel entry)",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let account_sessions = IntCounterVec::new(
            Opts::new(
                "payments_account_sessions_total",
                "Account Sessions minted for embedded onboarding, by outcome",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let _ = registry.register(Box::new(connection_creates.clone()));
        let _ = registry.register(Box::new(account_sessions.clone()));

        let webhook_lag_seconds = Histogram::with_opts(HistogramOpts::new(
            "payments_webhook_lag_seconds",
            "Connect webhook reception → processing lag in seconds",
        ))
        .expect("histogram construction is infallible");
        let _ = registry.register(Box::new(webhook_lag_seconds.clone()));
        let webhook_unprocessed = IntGauge::new(
            "payments_webhook_unprocessed_gauge",
            "Stored but unprocessed Connect webhook events",
        )
        .expect("gauge construction is infallible");
        let _ = registry.register(Box::new(webhook_unprocessed.clone()));
        let webhook_dead_letter = IntGauge::new(
            "payments_webhook_dead_letter_gauge",
            "Connect webhook events with an exhausted retry budget",
        )
        .expect("gauge construction is infallible");
        let _ = registry.register(Box::new(webhook_dead_letter.clone()));
        let webhook_events = IntCounterVec::new(
            Opts::new(
                "payments_webhook_events_total",
                "Connect webhook dispatch outcomes",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let _ = registry.register(Box::new(webhook_events.clone()));
        let webhook_unknown_account = IntCounter::new(
            "payments_webhook_unknown_account_total",
            "Connect webhook events for accounts Sanvi does not know",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(webhook_unknown_account.clone()));
        let connection_status = IntGaugeVec::new(
            Opts::new(
                "payments_connection_status_gauge",
                "Live provider connections by lifecycle status",
            ),
            &["status"],
        )
        .expect("gauge vec construction is infallible");
        let _ = registry.register(Box::new(connection_status.clone()));
        let connection_activations = IntCounter::new(
            "payments_connection_activations_total",
            "Connections reaching Active (connect onboarding completions)",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(connection_activations.clone()));
        let reconciliation_checked = IntCounter::new(
            "payments_reconciliation_checked_total",
            "Connections re-synced by the reconciliation job",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(reconciliation_checked.clone()));
        let reconciliation_updated = IntCounter::new(
            "payments_reconciliation_updated_total",
            "Connections changed by the reconciliation job (lost-webhook repairs)",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(reconciliation_updated.clone()));
        let checkout_creates = IntCounterVec::new(
            Opts::new(
                "payments_checkout_creates_total",
                "Checkout session creations by outcome (`checkout_created_total{result}` in the plan's vocabulary)",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let _ = registry.register(Box::new(checkout_creates.clone()));
        let payout_failures = IntCounter::new(
            "payments_payout_failures_total",
            "Failed payouts observed from payout.failed events (tenant money stuck)",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(payout_failures.clone()));
        let tax_compliance_lapsed = IntCounter::new(
            "payments_tax_compliance_lapsed_total",
            "Tax-compliance lapses detected while automatic tax is enabled",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(tax_compliance_lapsed.clone()));
        let payment_succeeded = IntCounter::new(
            "payments_payment_succeeded_total",
            "Payments recorded succeeded (webhook truth, transition-guarded)",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(payment_succeeded.clone()));
        let payment_failures = IntCounterVec::new(
            Opts::new(
                "payments_payment_failure_total",
                "Payments recorded failed, by the provider's failure code",
            ),
            &["code"],
        )
        .expect("counter vec construction is infallible");
        let _ = registry.register(Box::new(payment_failures.clone()));
        let refunds_issued = IntCounter::new(
            "payments_refunds_issued_total",
            "Refunds that landed succeeded (transition-guarded)",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(refunds_issued.clone()));
        let payouts_paid = IntCounter::new(
            "payments_payouts_paid_total",
            "Payouts confirmed arrived in the tenant's bank (payout.paid transitions)",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(payouts_paid.clone()));
        let dispute_open = IntGauge::new(
            "payments_dispute_open_gauge",
            "Disputes currently open (need response or under review)",
        )
        .expect("gauge construction is infallible");
        let _ = registry.register(Box::new(dispute_open.clone()));
        let platform_balance_movements = IntCounter::new(
            "payments_platform_balance_movements_total",
            "Platform-balance movements observed on tenant transactions — the invariant alarm; any value pages an operator",
        )
        .expect("counter construction is infallible");
        let _ = registry.register(Box::new(platform_balance_movements.clone()));
        let connection_disconnects = IntCounterVec::new(
            Opts::new(
                "payments_connection_disconnects_total",
                "Tenant disconnect attempts by outcome (`ok`, or the blocker that refused)",
            ),
            &["result"],
        )
        .expect("counter vec construction is infallible");
        let _ = registry.register(Box::new(connection_disconnects.clone()));

        Self {
            catalog_requests,
            entitlement_denied,
            connection_creates,
            account_sessions,
            webhook_lag_seconds,
            webhook_unprocessed,
            webhook_dead_letter,
            webhook_events,
            webhook_unknown_account,
            connection_status,
            connection_activations,
            reconciliation_checked,
            reconciliation_updated,
            checkout_creates,
            payout_failures,
            tax_compliance_lapsed,
            payment_succeeded,
            payment_failures,
            refunds_issued,
            payouts_paid,
            dispute_open,
            platform_balance_movements,
            connection_disconnects,
        }
    }

    /// Observe reception → processing lag for one event.
    pub fn observe_webhook_lag(&self, received_at: DateTime<Utc>, now: DateTime<Utc>) {
        let lag = (now - received_at).num_milliseconds();
        self.webhook_lag_seconds
            .observe((lag.max(0) as f64) / 1000.0);
    }
}
