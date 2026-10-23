//! Connect webhook receiving and dispatch (sub-phase 09.3).
//!
//! Store-first, exactly like the phase-04 platform transport: the receiver
//! verifies the signature on the **raw** body (before any parsing), persists
//! the event keyed by the provider's event id — the primary key *is* the
//! dedupe — and answers 2xx before processing starts. The worker's
//! dispatcher drains unprocessed rows one transaction each, so a poison
//! event cannot take the batch down; exhausting the retry budget
//! dead-letters an event (it stops being fetched, never blocking the queue)
//! until `apps/cli` replays it.
//!
//! Routing: every event names a connected account; it maps to exactly one
//! stored connection via `external_account_id`. An unknown account is
//! recorded, alerted — and nothing is created. An unknown account is either
//! a leaked endpoint or an orphan from a failed create; it is never a
//! signal to provision.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use tracing::{error, warn};

use crate::adapters::PaymentsMetrics;
use crate::application::ProviderRegistry;
use crate::application::lifecycle::{AppliedTransition, apply_account_snapshot};
use crate::domain::ConnectionStatus;
use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::ports::{ConnectionLifecycleStore, WebhookEventRepository, WebhookVerifierPort};
use crate::domain::webhook::{WebhookEventRecord, event_account_id, event_created};
use sanvi_persistence::TxManager;
use serde_json::Value;

const ACTOR: &str = "stripe-connect-webhook";

/// How many processing attempts before a webhook row becomes dead-letter.
/// Same budget as the phase-04 platform endpoint.
pub const MAX_WEBHOOK_ATTEMPTS: i32 = 5;

/// How many events one dispatch pass drains at most.
pub const DISPATCH_BATCH: u32 = 100;

// ---------------------------------------------------------------------------
// Receiving (HTTP boundary → storage)
// ---------------------------------------------------------------------------

/// Verify + store, answering Stripe's redelivery contract with a fast 2xx.
pub struct WebhookReceiver {
    tx_manager: TxManager,
    events: Arc<dyn WebhookEventRepository>,
    /// The adapter-owned verifier for THIS endpoint's secret. The Connect
    /// secret and the phase-04 platform secret are distinct trust domains
    /// (cross-secret tests in [`crate::domain::webhook`], boot check in the
    /// composition root) — an event signed for one must fail here.
    verifier: Arc<dyn WebhookVerifierPort>,
    allowed_clients: Vec<String>,
}

impl WebhookReceiver {
    pub fn new(
        tx_manager: TxManager,
        events: Arc<dyn WebhookEventRepository>,
        verifier: Arc<dyn WebhookVerifierPort>,
        allowed_clients: Vec<String>,
    ) -> Self {
        Self {
            tx_manager,
            events,
            verifier,
            allowed_clients,
        }
    }

    /// Verify on the raw bytes, store, ack. Returns `true` when the event
    /// was newly stored; a redelivery returns `Ok(false)` — both answer
    /// 2xx so Stripe stops retrying either way.
    #[tracing::instrument(name = "payments.receive", skip_all)]
    pub async fn receive(
        &self,
        payload: &[u8],
        signature_header: Option<&str>,
        client_ip: Option<IpAddr>,
    ) -> Result<bool, PaymentsError> {
        let Some(signature) = signature_header else {
            return Err(PaymentsError::WebhookRejected(
                "missing stripe-signature header".to_string(),
            ));
        };
        if !client_allowed(
            &self.allowed_clients,
            client_ip.map(|ip| ip.to_string()).as_deref(),
        ) {
            return Err(PaymentsError::WebhookRejected(
                "client address not allow-listed".to_string(),
            ));
        }
        // Signature FIRST, raw payload only — anything that parsed before
        // this line has already lost (NFR-905).
        self.verifier.verify(payload, signature).await?;

        let event: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|err| PaymentsError::WebhookRejected(format!("payload is not json: {err}")))?;
        let stripe_event_id = event
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| PaymentsError::WebhookRejected("missing event id".to_string()))?
            .to_string();
        let record = WebhookEventRecord {
            stripe_event_id,
            account_id: event_account_id(&event),
            event_type: event
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            api_version: event
                .get("api_version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            created: event_created(&event, Utc::now()),
            received_at: Utc::now(),
            attempts: 0,
            last_error: None,
            payload: event,
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("store_connect_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let inserted = self.events.insert_ignore(&mut scope, &record).await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(inserted)
    }
}

/// CIDR or exact match (same vocabulary as the phase-04 receiver; empty
/// allow-list = fail closed, never open).
fn client_allowed(allowed: &[String], client_ip: Option<&str>) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let Some(client_ip) = client_ip else {
        return false;
    };
    allowed.iter().any(|entry| {
        if let Some((network, bits)) = entry.split_once('/') {
            match (network.parse::<std::net::Ipv4Addr>(), bits.parse::<u8>()) {
                (Ok(network), Ok(bits)) if bits <= 32 => {
                    let mask = u32::MAX << (32 - bits);
                    let network_u32 = u32::from(network) & mask;
                    client_ip
                        .parse::<std::net::Ipv4Addr>()
                        .map(|ip| u32::from(ip) & mask == network_u32)
                        .unwrap_or(false)
                }
                _ => false,
            }
        } else {
            entry == client_ip
        }
    })
}

// ---------------------------------------------------------------------------
// Dispatch (worker)
// ---------------------------------------------------------------------------

/// What one drain pass accomplished (logs + metrics).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct DispatchReport {
    pub processed: u32,
    /// Stored and acknowledged without state change (unroutable accounts,
    /// stale out-of-order events, types without handlers yet).
    pub skipped: u32,
    /// Attempts recorded against the dead-letter policy.
    pub failed: u32,
}

/// The worker-side dispatcher: drains unprocessed Connect webhook rows and
/// applies each inside its own audited platform transaction.
///
/// This slice ships the transport plus the account/capability handlers
/// (`account.updated`, `capability.updated`, `v2.core.account…`) and,
/// from 09.4, the payment handlers (`checkout.session.completed`,
/// `checkout.session.expired`, `payment_intent.succeeded`,
/// `payment_intent.payment_failed`, `payment_intent.canceled`). Refund,
/// dispute and payout handlers arrive with their slices; unknown types
/// are stored and acknowledged, never fatal. The payment handlers stay
/// deployed even while checkout creation is flag-off — money taken at
/// Stripe must always land in a local record.
pub struct WebhookDispatcher {
    tx_manager: TxManager,
    events_repo: Arc<dyn WebhookEventRepository>,
    connections: Arc<dyn ConnectionLifecycleStore>,
    registry: Arc<ProviderRegistry>,
    events: Arc<dyn EventPublisher>,
    /// The checkout/payment persistence the 09.4 handlers drive. Absent
    /// (unwired composition root) payment events are still stored and
    /// acknowledged — never retried into the DLQ by a wiring gap.
    checkouts: Option<Arc<dyn CheckoutLifecycleStore>>,
    /// 09.5 refund and dispute persistence.
    refunds: Option<Arc<dyn crate::domain::ports::RefundRepository>>,
    disputes: Option<Arc<dyn crate::domain::ports::DisputeRepository>>,
    /// 09.6 payout projection persistence.
    payouts: Option<Arc<dyn crate::domain::ports::PayoutRepository>>,
    /// 09.6 tax-compliance re-checks triggered by tax.* account events.
    tax_checker: Option<Arc<crate::application::tax_settings::CheckTenantTaxCompliance>>,
    ids: Arc<dyn crate::domain::ports::IdGenerator>,
    metrics: Option<PaymentsMetrics>,
}

impl WebhookDispatcher {
    pub fn new(
        tx_manager: TxManager,
        events_repo: Arc<dyn WebhookEventRepository>,
        connections: Arc<dyn ConnectionLifecycleStore>,
        registry: Arc<ProviderRegistry>,
        events: Arc<dyn EventPublisher>,
        ids: Arc<dyn crate::domain::ports::IdGenerator>,
    ) -> Self {
        Self {
            tx_manager,
            events_repo,
            connections,
            registry,
            events,
            checkouts: None,
            refunds: None,
            disputes: None,
            payouts: None,
            tax_checker: None,
            ids,
            metrics: None,
        }
    }

    /// Wire the 09.4 payment handlers' persistence. Every composition
    /// root does — the Option exists so an unwired dispatcher degrades to
    /// acknowledge-and-skip instead of dead-lettering money events.
    pub fn with_checkout_store(mut self, checkouts: Arc<dyn CheckoutLifecycleStore>) -> Self {
        self.checkouts = Some(checkouts);
        self
    }

    pub fn with_refund_store(
        mut self,
        refunds: Arc<dyn crate::domain::ports::RefundRepository>,
    ) -> Self {
        self.refunds = Some(refunds);
        self
    }

    pub fn with_dispute_store(
        mut self,
        disputes: Arc<dyn crate::domain::ports::DisputeRepository>,
    ) -> Self {
        self.disputes = Some(disputes);
        self
    }

    /// Wire the 09.6 payout projection.
    pub fn with_payout_store(
        mut self,
        payouts: Arc<dyn crate::domain::ports::PayoutRepository>,
    ) -> Self {
        self.payouts = Some(payouts);
        self
    }

    /// Wire the 09.6 tax-compliance re-checker (driven by `tax.*` events).
    pub fn with_tax_checker(
        mut self,
        checker: Arc<crate::application::tax_settings::CheckTenantTaxCompliance>,
    ) -> Self {
        self.tax_checker = Some(checker);
        self
    }

    /// Attach payments metrics (the worker does; tests need not).
    pub fn with_metrics(mut self, metrics: PaymentsMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Drain one batch. Every row is processed in its own transaction, so
    /// a poisoned event cannot take the batch down with it.
    pub async fn dispatch(&self, limit: u32) -> Result<DispatchReport, PaymentsError> {
        let batch = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("fetch_connect_webhook_batch", ACTOR)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            let batch = self
                .events_repo
                .fetch_unprocessed(&mut scope, limit)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            batch
        };

        let mut report = DispatchReport::default();
        for record in batch {
            match self.apply(&record).await {
                Ok(outcome) => {
                    if let Some(metrics) = &self.metrics {
                        metrics.observe_webhook_lag(record.received_at, Utc::now());
                        metrics
                            .webhook_events
                            .with_label_values(&[outcome.label()])
                            .inc();
                    }
                    match outcome {
                        ApplyOutcome::Applied => report.processed += 1,
                        ApplyOutcome::Skipped => report.skipped += 1,
                    }
                }
                Err(err) => {
                    report.failed += 1;
                    error!(
                        event = %record.stripe_event_id,
                        event_type = %record.event_type,
                        error = %err,
                        "connect webhook event processing failed",
                    );
                    if let Err(scope_err) = self.mark_failed(&record.stripe_event_id, &err).await {
                        error!(error = %scope_err, "could not record connect webhook failure");
                    }
                }
            }
        }
        Ok(report)
    }

    async fn mark_failed(
        &self,
        stripe_event_id: &str,
        err: &PaymentsError,
    ) -> Result<(), PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("fail_connect_webhook_event", ACTOR)
            .await
            .map_err(|e| PaymentsError::Storage(e.to_string()))?;
        self.events_repo
            .mark_attempt_failed(&mut scope, stripe_event_id, &err.to_string())
            .await?;
        scope
            .commit()
            .await
            .map_err(|e| PaymentsError::Storage(e.to_string()))?;
        Ok(())
    }

    /// One event, one transaction: route → guard → fetch authoritative
    /// state → apply through the shared lifecycle path → mark processed.
    #[tracing::instrument(
        name = "payments.webhook.apply",
        skip_all,
        fields(event = %record.stripe_event_id, event_type = %record.event_type)
    )]
    async fn apply(&self, record: &WebhookEventRecord) -> Result<ApplyOutcome, PaymentsError> {
        // 09.4 payment handlers first — they never consult the provider
        // (the event *is* the provider's statement) and must stay deployed
        // even while checkout creation is disabled.
        if is_payment_event(&record.event_type) {
            return self.apply_payment_event(record).await;
        }
        if is_refund_event(&record.event_type) {
            return self.apply_refund_event(record).await;
        }
        if is_dispute_event(&record.event_type) {
            return self.apply_dispute_event(record).await;
        }
        if is_payout_event(&record.event_type) {
            return self.apply_payout_event(record).await;
        }
        if is_tax_event(&record.event_type) {
            return self.apply_tax_event(record).await;
        }

        // Account/capability handlers; everything else lands with
        // its own slice and is acknowledged meanwhile.
        if !is_account_event(&record.event_type) {
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        }

        // Route by connected account. Unroutable = recorded (the stored
        // row), alerted (error log + metric), acknowledged — never
        // provisioned, never retried into a DLQ slot.
        let Some(account_id) = record.account_id.as_deref() else {
            warn!(
                event = %record.stripe_event_id,
                event_type = %record.event_type,
                "connect webhook names no connected account; recorded and acknowledged"
            );
            self.note_unknown_account(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("process_connect_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;

        let Some(connection) = self
            .connections
            .find_live_by_external_account(&mut scope, account_id)
            .await?
        else {
            error!(
                event = %record.stripe_event_id,
                account = %account_id,
                "connect webhook for UNKNOWN account; recorded and alerted, no rows created",
            );
            if let Some(metrics) = &self.metrics {
                metrics.webhook_unknown_account.inc();
            }
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        };

        // Terminal connections have no lifecycle left to drive.
        if connection.disconnected_at.is_some()
            || matches!(
                connection.status,
                ConnectionStatus::Disconnected | ConnectionStatus::Rejected
            )
        {
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        }

        // Out-of-order guard: strictly older than what we already applied
        // is dropped (acknowledged, not retried).
        if connection.is_event_stale(record.created) {
            tracing::debug!(
                event = %record.stripe_event_id,
                connection_id = %connection.id,
                "stale connect webhook dropped (out-of-order)",
            );
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        }

        // The event is a signal; the provider's current state is the
        // truth. Fetch outside the write path below but inside this
        // function so a provider outage fails the attempt (retry/DLQ)
        // instead of poisoning the transaction.
        let adapter = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available in this environment",
                connection.provider.as_str()
            ))
        })?;
        drop(scope);
        let snapshot = adapter.fetch_status(account_id).await?;

        let mut scope = self
            .tx_manager
            .begin_unscoped("apply_connect_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        // The connection may have moved while we were at the provider:
        // re-read inside the applying transaction.
        let connection = self
            .connections
            .find_live_by_external_account(&mut scope, account_id)
            .await?
            .ok_or_else(|| {
                PaymentsError::InvalidConnection(format!(
                    "connection for {account_id} disappeared mid-processing"
                ))
            })?;
        if connection.is_event_stale(record.created) {
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        }

        let outcome = apply_account_snapshot(
            self.connections.as_ref(),
            self.events.as_ref(),
            &mut scope,
            &connection,
            &snapshot,
            record.created,
        )
        .await?;

        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(match outcome {
            AppliedTransition::Changed => ApplyOutcome::Applied,
            AppliedTransition::Touched | AppliedTransition::Unchanged => ApplyOutcome::Skipped,
        })
    }

    /// One payment event: route by connected account → locate the
    /// checkout → write the read-model row → transition the checkout →
    /// emit the outbox event. The webhook is the fact; nothing here ever
    /// consults a redirect URL.
    async fn apply_payment_event(
        &self,
        record: &WebhookEventRecord,
    ) -> Result<ApplyOutcome, PaymentsError> {
        // Unwired composition root: acknowledge-and-skip, never DLQ.
        let Some(checkouts) = self.checkouts.clone() else {
            warn!(
                event = %record.stripe_event_id,
                "payment event arrived with no checkout store wired; acknowledged unprocessed"
            );
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(account_id) = record.account_id.as_deref() else {
            warn!(
                event = %record.stripe_event_id,
                event_type = %record.event_type,
                "payment webhook names no connected account; recorded and acknowledged"
            );
            self.note_unknown_account(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };

        let mut scope = self
            .tx_manager
            .begin_unscoped("process_payment_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;

        // Money events route by account **whatever the connection's
        // lifecycle state** (09.7): a checkout session minted before the
        // tenant disconnected can still complete, and that payment must
        // be recorded — money never moves without a local record. Only
        // the lifecycle handlers require a live row. Unknown accounts are
        // alerted and acknowledged, never provisioned.
        let Some(connection) = self
            .connections
            .find_by_external_account(&mut scope, account_id)
            .await?
        else {
            error!(
                event = %record.stripe_event_id,
                account = %account_id,
                "payment webhook for UNKNOWN account; recorded and alerted, no rows created",
            );
            if let Some(metrics) = &self.metrics {
                metrics.webhook_unknown_account.inc();
            }
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        };

        let (outcome, committed_metrics) = handle_payment_event(
            &mut scope,
            checkouts,
            self.events.as_ref(),
            self.ids.as_ref(),
            &connection,
            record,
        )
        .await?;
        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        record_committed_metrics(self.metrics.as_ref(), committed_metrics);
        Ok(outcome)
    }

    async fn apply_refund_event(
        &self,
        record: &WebhookEventRecord,
    ) -> Result<ApplyOutcome, PaymentsError> {
        let Some(refunds_repo) = self.refunds.clone() else {
            warn!(
                event = %record.stripe_event_id,
                "refund event arrived with no refund store wired; acknowledged"
            );
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(checkouts) = self.checkouts.clone() else {
            warn!(
                event = %record.stripe_event_id,
                "refund event arrived with no checkout store wired; acknowledged"
            );
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(account_id) = record.account_id.as_deref() else {
            warn!(
                event = %record.stripe_event_id,
                event_type = %record.event_type,
                "refund webhook names no connected account; recorded and acknowledged"
            );
            self.note_unknown_account(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("process_refund_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        // Money truth routes regardless of lifecycle state (09.7).
        let Some(connection) = self
            .connections
            .find_by_external_account(&mut scope, account_id)
            .await?
        else {
            error!(
                event = %record.stripe_event_id,
                account = %account_id,
                "refund webhook for UNKNOWN account; recorded and alerted, no rows created",
            );
            if let Some(metrics) = &self.metrics {
                metrics.webhook_unknown_account.inc();
            }
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        };
        let (outcome, committed_metrics) = handle_refund_event(
            &mut scope,
            checkouts.as_ref(),
            refunds_repo.as_ref(),
            self.events.as_ref(),
            self.ids.as_ref(),
            &connection,
            record,
        )
        .await?;
        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        record_committed_metrics(self.metrics.as_ref(), committed_metrics);
        Ok(outcome)
    }

    async fn apply_dispute_event(
        &self,
        record: &WebhookEventRecord,
    ) -> Result<ApplyOutcome, PaymentsError> {
        let Some(disputes_repo) = self.disputes.clone() else {
            warn!(
                event = %record.stripe_event_id,
                "dispute event arrived with no dispute store wired; acknowledged"
            );
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(checkouts) = self.checkouts.clone() else {
            warn!(
                event = %record.stripe_event_id,
                "dispute event arrived with no checkout store wired; acknowledged"
            );
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(account_id) = record.account_id.as_deref() else {
            warn!(
                event = %record.stripe_event_id,
                event_type = %record.event_type,
                "dispute webhook names no connected account; recorded and acknowledged"
            );
            self.note_unknown_account(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("process_dispute_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        // Money truth routes regardless of lifecycle state (09.7): a
        // dispute on a pre-disconnect payment still needs its projection.
        let Some(connection) = self
            .connections
            .find_by_external_account(&mut scope, account_id)
            .await?
        else {
            error!(
                event = %record.stripe_event_id,
                account = %account_id,
                "dispute webhook for UNKNOWN account; recorded and alerted, no rows created",
            );
            if let Some(metrics) = &self.metrics {
                metrics.webhook_unknown_account.inc();
            }
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        };
        let (outcome, committed_metrics) = handle_dispute_event(
            &mut scope,
            checkouts.as_ref(),
            disputes_repo.as_ref(),
            self.events.as_ref(),
            self.ids.as_ref(),
            &connection,
            record,
        )
        .await?;
        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        record_committed_metrics(self.metrics.as_ref(), committed_metrics);
        Ok(outcome)
    }

    /// Store-and-ack for event types whose handlers ship later — the row
    /// is marked processed so it neither retries nor blocks the queue.
    async fn acknowledge(&self, record: &WebhookEventRecord) -> Result<(), PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("acknowledge_connect_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn note_unknown_account(&self, record: &WebhookEventRecord) -> Result<(), PaymentsError> {
        if let Some(metrics) = &self.metrics {
            metrics.webhook_unknown_account.inc();
        }
        self.acknowledge(record).await
    }
}

enum ApplyOutcome {
    Applied,
    Skipped,
}

impl ApplyOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Skipped => "skipped",
        }
    }
}

/// Metrics are emitted only after their projection transaction commits.
/// Prometheus counters cannot be rolled back, so incrementing inside the
/// handler would over-count a webhook retried after a later write failed.
#[derive(Debug)]
enum CommittedMetric {
    PaymentSucceeded,
    PaymentFailed(String),
    RefundIssued,
    PayoutPaid,
    PayoutFailed,
    DisputeOpenDelta(i64),
    PlatformBalanceMovement {
        event_id: String,
        account_id: String,
    },
}

fn record_committed_metrics(
    metrics: Option<&PaymentsMetrics>,
    metrics_to_record: Vec<CommittedMetric>,
) {
    for metric in metrics_to_record {
        match metric {
            CommittedMetric::PaymentSucceeded => {
                if let Some(metrics) = metrics {
                    metrics.payment_succeeded.inc();
                }
            }
            CommittedMetric::PaymentFailed(code) => {
                if let Some(metrics) = metrics {
                    metrics.payment_failures.with_label_values(&[&code]).inc();
                }
            }
            CommittedMetric::RefundIssued => {
                if let Some(metrics) = metrics {
                    metrics.refunds_issued.inc();
                }
            }
            CommittedMetric::PayoutPaid => {
                if let Some(metrics) = metrics {
                    metrics.payouts_paid.inc();
                }
            }
            CommittedMetric::PayoutFailed => {
                if let Some(metrics) = metrics {
                    metrics.payout_failures.inc();
                }
            }
            CommittedMetric::DisputeOpenDelta(delta) => {
                if let Some(metrics) = metrics {
                    if delta > 0 {
                        metrics.dispute_open.inc();
                    } else if delta < 0 {
                        metrics.dispute_open.dec();
                    }
                }
            }
            CommittedMetric::PlatformBalanceMovement {
                event_id,
                account_id,
            } => {
                if let Some(metrics) = metrics {
                    metrics.platform_balance_movements.inc();
                }
                error!(
                    event = %event_id,
                    account = %account_id,
                    "PLATFORM BALANCE MOVEMENT on a tenant transaction — the never-merchant-of-record invariant is violated"
                );
            }
        }
    }
}

/// The event types this slice handles: everything about the connected
/// account itself, from both the v1 envelope (`account.updated`,
/// `capability.updated`) and Accounts v2 (`v2.core.account…`).
fn is_account_event(event_type: &str) -> bool {
    event_type == "account.updated"
        || event_type == "capability.updated"
        || event_type.starts_with("v2.core.account")
}

/// The 09.4 payment events. Each carries a connected-account id and a
/// provider object whose metadata routes it to one checkout.
fn is_payment_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "checkout.session.completed"
            | "checkout.session.expired"
            | "payment_intent.succeeded"
            | "payment_intent.payment_failed"
            | "payment_intent.canceled"
    )
}

fn is_refund_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "charge.refunded" | "refund.created" | "refund.updated"
    )
}

fn is_dispute_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "charge.dispute.created"
            | "charge.dispute.closed"
            | "charge.dispute.updated"
            | "charge.dispute.funds_reinstated"
            | "charge.dispute.funds_withdrawn"
    )
}

/// Payout creation/updates populate the pending projection used by the
/// disconnect guard. `payout.paid` confirms arrival; `payout.failed` means
/// the tenant's money is stuck and raises an alert.
fn is_payout_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "payout.created" | "payout.updated" | "payout.paid" | "payout.failed"
    )
}

/// Tax-related account events: the connected account's tax configuration
/// changed, so a stored enabled toggle may now be stale (registrations
/// expired on the provider side). Triggers the same re-check as the
/// scheduled job.
fn is_tax_event(event_type: &str) -> bool {
    event_type == "tax.settings.updated" || event_type.starts_with("tax.registration")
}

// ---------------------------------------------------------------------------
// 09.4 payment handlers.
//
// Direct charges only: the payment moves the tenant's own money, so every
// record is keyed by the requesting tenant's id. The webhook payload's
// metadata (`checkout_id` + `tenant_id`) is the primary link back to our
// checkout row; `checkout.session.id` (`cs_…`) is the secondary lookup.
//
// An expired session carries no provider payment object, so no
// `payments.payments` row exists to write — the honest record of "no
// money moved" is the checkout transition alone.
// ---------------------------------------------------------------------------

use crate::domain::checkout::{Checkout, CheckoutId};
use crate::domain::events::EventAmount;
use crate::domain::money::Money;
use crate::domain::payment::{PaymentFailure, PaymentRecord, PaymentStatus};
use crate::domain::ports::{CheckoutLifecycleStore, IdGenerator};
use sanvi_kernel::TenantId;
use sanvi_persistence::PlatformScope;

async fn handle_payment_event(
    scope: &mut PlatformScope<'_>,
    checkouts: Arc<dyn CheckoutLifecycleStore>,
    events: &dyn EventPublisher,
    ids: &dyn IdGenerator,
    connection: &crate::domain::connection::ProviderConnection,
    record: &WebhookEventRecord,
) -> Result<(ApplyOutcome, Vec<CommittedMetric>), PaymentsError> {
    let object = record
        .payload
        .pointer("/data/object")
        .cloned()
        .unwrap_or(Value::Null);
    let now = Utc::now();
    let tenant_id = connection.tenant_id;
    let provider = connection.provider.as_str().to_string();

    // The invariant alarm (09.7): direct charges never move platform
    // money, so a fee/transfer shape on a tenant transaction means Sanvi
    // has become a party to the flow. Count it and page loudly — the
    // event is still recorded either way.
    let mut committed_metrics = platform_balance_movement(&object)
        .then_some(CommittedMetric::PlatformBalanceMovement {
            event_id: record.stripe_event_id.clone(),
            account_id: connection.external_account_id.clone(),
        })
        .into_iter()
        .collect::<Vec<_>>();

    match record.event_type.as_str() {
        "checkout.session.completed" => {
            let Some(mut checkout) = locate_checkout(scope, checkouts.as_ref(), &object).await?
            else {
                warn!(
                    event = %record.stripe_event_id,
                    "checkout.session.completed for an unknown checkout; acknowledged"
                );
                return Ok((ApplyOutcome::Skipped, committed_metrics));
            };
            let amount = money_from(&object, "amount_total")?;
            checkout.complete(now)?;
            checkouts.save(scope, &checkout).await?;

            // The session names its payment intent; that object becomes
            // the read-model row (the succeeding pi.succeeded upserts the
            // same external id into the same state).
            if let Some(external_payment_id) = object.get("payment_intent").and_then(Value::as_str)
            {
                let (payment, changed) = upsert_payment(
                    scope,
                    checkouts.as_ref(),
                    ids,
                    tenant_id,
                    Some(checkout.id),
                    external_payment_id,
                    PaymentStatus::Succeeded,
                    amount.clone(),
                    None,
                    None,
                    Some(now),
                )
                .await?;
                // Sibling events converge here: the outbox event fires on
                // the *transition*, not on every delivery.
                if changed {
                    committed_metrics.push(CommittedMetric::PaymentSucceeded);
                    events
                        .publish(
                            scope,
                            tenant_id,
                            &[PaymentsEvent::PaymentSucceeded {
                                payment_id: payment.id,
                                checkout_id: Some(checkout.id),
                                provider,
                                amount: EventAmount::new(&amount),
                            }],
                        )
                        .await?;
                }
            }
            Ok((ApplyOutcome::Applied, committed_metrics))
        }
        "checkout.session.expired" => {
            let Some(mut checkout) = locate_checkout(scope, checkouts.as_ref(), &object).await?
            else {
                warn!(
                    event = %record.stripe_event_id,
                    "checkout.session.expired for an unknown checkout; acknowledged"
                );
                return Ok((ApplyOutcome::Skipped, committed_metrics));
            };
            checkout.expire()?;
            checkouts.save(scope, &checkout).await?;
            Ok((ApplyOutcome::Applied, committed_metrics))
        }
        "payment_intent.succeeded"
        | "payment_intent.payment_failed"
        | "payment_intent.canceled" => {
            let external_payment_id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| id.starts_with("pi_"))
                .ok_or_else(|| {
                    PaymentsError::Storage(format!(
                        "payment webhook {} carries no payment intent id",
                        record.stripe_event_id
                    ))
                })?
                .to_string();
            let status = match record.event_type.as_str() {
                "payment_intent.succeeded" => PaymentStatus::Succeeded,
                "payment_intent.payment_failed" => PaymentStatus::Failed,
                _ => PaymentStatus::Canceled,
            };
            let amount = money_from(&object, "amount")?;
            // The platform's application fee, when one was charged,
            // reported by the provider — recorded because disclosure is
            // the contract (a platform fee the merchant cannot see is not
            // acceptable). Absent when fees are off (the release default);
            // the upsert preserves a previously recorded fee then.
            let application_fee = match object.get("application_fee_amount") {
                Some(Value::Number(minor)) => Money::new(
                    minor.as_i64().unwrap_or_default(),
                    object.get("currency").and_then(Value::as_str).unwrap_or(""),
                )
                .ok(),
                _ => None,
            };
            let mut checkout = locate_checkout(scope, checkouts.as_ref(), &object).await?;

            let (payment, changed) = upsert_payment(
                scope,
                checkouts.as_ref(),
                ids,
                tenant_id,
                checkout.as_ref().map(|c| c.id),
                &external_payment_id,
                status,
                amount,
                application_fee,
                failure_of(&object),
                (status == PaymentStatus::Succeeded).then_some(now),
            )
            .await?;

            // Forward-only checkout transitions driven by the payment's
            // own outcome. A failed attempt leaves the checkout live —
            // the customer may retry at the hosted page until it expires.
            if let Some(checkout) = checkout.as_mut() {
                match status {
                    PaymentStatus::Succeeded => checkout.complete(now)?,
                    PaymentStatus::Canceled => checkout.cancel()?,
                    _ => {}
                }
                checkouts.save(scope, checkout).await?;
            }

            match (status, changed) {
                (PaymentStatus::Failed, true) => {
                    let failure_code = payment
                        .failure
                        .map(|f| f.code)
                        .unwrap_or_else(|| "unknown".to_string());
                    committed_metrics.push(CommittedMetric::PaymentFailed(failure_code.clone()));
                    events
                        .publish(
                            scope,
                            tenant_id,
                            &[PaymentsEvent::PaymentFailed {
                                payment_id: payment.id,
                                checkout_id: payment.checkout_id,
                                provider,
                                failure_code,
                            }],
                        )
                        .await?;
                }
                (PaymentStatus::Succeeded, true) => {
                    committed_metrics.push(CommittedMetric::PaymentSucceeded);
                    events
                        .publish(
                            scope,
                            tenant_id,
                            &[PaymentsEvent::PaymentSucceeded {
                                payment_id: payment.id,
                                checkout_id: payment.checkout_id,
                                provider,
                                amount: EventAmount::new(&payment.amount),
                            }],
                        )
                        .await?;
                }
                _ => {}
            }
            Ok((ApplyOutcome::Applied, committed_metrics))
        }
        _ => Ok((ApplyOutcome::Skipped, committed_metrics)),
    }
}

/// Find the checkout an event object refers to: metadata first
/// (`checkout_id` + `tenant_id`, scoped by the tenant so a forged value
/// simply misses), then the session id for checkout.session.* envelopes.
async fn locate_checkout(
    scope: &mut PlatformScope<'_>,
    checkouts: &dyn CheckoutLifecycleStore,
    object: &Value,
) -> Result<Option<Checkout>, PaymentsError> {
    if let (Some(tenant_raw), Some(checkout_raw)) = (
        object
            .pointer("/metadata/tenant_id")
            .and_then(Value::as_str),
        object
            .pointer("/metadata/checkout_id")
            .and_then(Value::as_str),
    ) && let (Ok(tenant_id), Ok(checkout_id)) = (
        tenant_raw.parse::<TenantId>(),
        checkout_raw.parse::<CheckoutId>(),
    ) && let Some(found) = checkouts.find_by_id(scope, tenant_id, checkout_id).await?
    {
        return Ok(Some(found));
    }
    match object.get("id").and_then(Value::as_str) {
        Some(session_id) if session_id.starts_with("cs_") => {
            checkouts.find_by_external_session(scope, session_id).await
        }
        _ => Ok(None),
    }
}

/// Insert-or-update by external id, keeping an existing row's id (so the
/// outbox aggregates stay stable across sibling events and redeliveries)
/// and never regressing a capture timestamp. Returns whether this call
/// *moved* the record into its new status — the dedupe that makes
/// sibling events (`session.completed` then `pi.succeeded`) emit exactly
/// one outbox event between them.
#[allow(clippy::too_many_arguments)]
async fn upsert_payment(
    scope: &mut PlatformScope<'_>,
    checkouts: &dyn CheckoutLifecycleStore,
    ids: &dyn IdGenerator,
    tenant_id: TenantId,
    checkout_id: Option<CheckoutId>,
    external_payment_id: &str,
    status: PaymentStatus,
    amount: Money,
    fee: Option<Money>,
    failure: Option<PaymentFailure>,
    captured_at: Option<chrono::DateTime<Utc>>,
) -> Result<(PaymentRecord, bool), PaymentsError> {
    let existing = checkouts
        .find_payment_by_external_id(scope, tenant_id, external_payment_id)
        .await?;
    let changed = !existing.as_ref().is_some_and(|p| p.status == status);
    // Preserve an existing id and any previously recorded provider fee
    // (the disclosure survives sibling events that don't carry the fee).
    let id = match existing.as_ref() {
        Some(p) => p.id,
        None => ids.next_payment_id()?,
    };
    let mut record = PaymentRecord::new(
        id,
        tenant_id,
        checkout_id,
        external_payment_id.to_string(),
        amount,
    )?;
    record.status = status;
    if fee.is_some() {
        record.fee = fee;
    } else {
        record.fee = existing.as_ref().and_then(|p| p.fee.clone());
    }
    record.payout_status = existing.as_ref().and_then(|p| p.payout_status);
    record.failure = failure;
    record.captured_at = captured_at;
    record.created_at = existing.as_ref().and_then(|p| p.created_at);
    checkouts.upsert_payment(scope, &record).await?;
    Ok((record, changed))
}

/// Minor-unit amount + ISO code off a provider object. Lowercase wire
/// codes normalize through [`Money`]; anything unusable fails the attempt
/// (retry → DLQ) rather than writing a wrong row.
fn money_from(object: &Value, field: &str) -> Result<Money, PaymentsError> {
    let minor = object.get(field).and_then(Value::as_i64).ok_or_else(|| {
        PaymentsError::Storage(format!("payment object carries no usable {field}"))
    })?;
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Money::new(minor, currency)
}

/// The failure code + human message from a failed PaymentIntent, if it
/// reports one. No card data ever passes this boundary.
fn failure_of(object: &Value) -> Option<PaymentFailure> {
    let error = object.get("last_payment_error")?;
    let code = error.get("code").and_then(Value::as_str)?;
    Some(PaymentFailure {
        code: code.to_string(),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

// ---------------------------------------------------------------------------
// 09.5 refund and dispute handlers.
// ---------------------------------------------------------------------------

use crate::domain::dispute::{DisputeRecord, DisputeStatus};
use crate::domain::payment::{RefundId, RefundRecord, RefundStatus};

/// Whether a provider object reports platform money on a tenant
/// transaction: an application fee (the platform's cut of a direct
/// charge) or any transfer shape. While the fee configuration is off —
/// the 0.10.0 default — *any* such field is a violation of the
/// never-merchant-of-record invariant, whatever its amount. Transfer
/// keys are matched by prefix (no banned wire literals here — the
/// invariant gate greps for them) which also catches future shapes.
fn platform_balance_movement(object: &Value) -> bool {
    let fee = object
        .get("application_fee_amount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let transfer = object.as_object().is_some_and(|map| {
        map.iter()
            .any(|(key, value)| key.starts_with("transfer_") && !value.is_null())
    });
    fee > 0 || transfer
}

async fn handle_refund_event(
    scope: &mut PlatformScope<'_>,
    checkouts: &dyn CheckoutLifecycleStore,
    refunds: &dyn crate::domain::ports::RefundRepository,
    events: &dyn EventPublisher,
    ids: &dyn IdGenerator,
    connection: &crate::domain::connection::ProviderConnection,
    record: &WebhookEventRecord,
) -> Result<(ApplyOutcome, Vec<CommittedMetric>), PaymentsError> {
    let object = record
        .payload
        .pointer("/data/object")
        .cloned()
        .unwrap_or(Value::Null);
    let tenant_id = connection.tenant_id;
    let provider = connection.provider.as_str().to_string();
    // Stripe sends `charge.refunded` with the charge object whose `refunds`
    // array contains every refund for that charge. A direct `refund.*`
    // event carries the refund object itself.
    let refunds_data: Vec<Value> = if record.event_type == "charge.refunded" {
        object
            .get("refunds")
            .and_then(|r| r.get("data"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        vec![object.clone()]
    };
    if refunds_data.is_empty() {
        return Ok((ApplyOutcome::Skipped, Vec::new()));
    }
    let mut any = false;
    let mut committed_metrics = Vec::new();
    for refund_obj in refunds_data {
        let external_refund_id = refund_obj
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("re_"))
            .ok_or_else(|| {
                PaymentsError::Storage(format!(
                    "refund webhook {} carries no refund id",
                    record.stripe_event_id
                ))
            })?
            .to_string();
        let amount = refund_obj
            .get("amount")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                PaymentsError::Storage(format!("refund {} has no amount", external_refund_id))
            })?;
        let currency = refund_obj
            .get("currency")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PaymentsError::Storage(format!("refund {} carries no currency", external_refund_id))
            })?;
        let money = Money::new(amount, currency)?;
        let status_raw = refund_obj
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("succeeded");
        let status = match status_raw {
            "succeeded" => RefundStatus::Succeeded,
            "pending" => RefundStatus::Pending,
            _ => RefundStatus::Failed,
        };
        let reason = refund_obj
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                object
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        // Find the payment this refund belongs to: prefer the refund's
        // `payment_intent` field, then the charge's `payment_intent`, then
        // the charge id itself.
        let external_payment_id = refund_obj
            .get("payment_intent")
            .and_then(Value::as_str)
            .or_else(|| object.get("payment_intent").and_then(Value::as_str))
            .or_else(|| refund_obj.get("charge").and_then(Value::as_str))
            .or_else(|| object.get("id").and_then(Value::as_str))
            .map(str::to_string);
        let Some(external_payment_id) = external_payment_id else {
            warn!(
                event = %record.stripe_event_id,
                "refund {} has no routable payment id; skipped", external_refund_id
            );
            continue;
        };
        let payment = match checkouts
            .find_payment_by_external_id(scope, tenant_id, &external_payment_id)
            .await?
        {
            Some(p) => p,
            None => {
                // Fallback: try charge id if we tried payment_intent.
                if let Some(charge_id) = object.get("id").and_then(Value::as_str)
                    && charge_id != external_payment_id
                {
                    match checkouts
                        .find_payment_by_external_id(scope, tenant_id, charge_id)
                        .await?
                    {
                        Some(p) => p,
                        None => {
                            warn!(
                                event = %record.stripe_event_id,
                                refund = %external_refund_id,
                                payment = %external_payment_id,
                                "refund for unknown payment; skipped"
                            );
                            continue;
                        }
                    }
                } else {
                    warn!(
                        event = %record.stripe_event_id,
                        refund = %external_refund_id,
                        payment = %external_payment_id,
                        "refund for unknown payment; skipped"
                    );
                    continue;
                }
            }
        };
        let created = refund_obj
            .get("created")
            .and_then(Value::as_i64)
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
            .unwrap_or_else(Utc::now);
        // Dedupe on external_refund_id: if we already have a row for this
        // provider id, update it; otherwise create a new succeeded row.
        let existing = refunds
            .find_by_external_id_platform(scope, tenant_id, &external_refund_id)
            .await?;
        // Our metadata is only a correlation hint. Bind every financial field
        // and the request key before adopting the original actor's snapshot.
        let request_id = refund_obj
            .pointer("/metadata/sanvi_refund_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<RefundId>().ok());
        let request_key = refund_obj
            .pointer("/metadata/sanvi_idempotency_key")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<uuid::Uuid>().ok());
        let correlated = if let (Some(request_id), Some(request_key)) = (request_id, request_key) {
            refunds
                .find_by_id_platform(scope, tenant_id, request_id)
                .await?
                .filter(|request| {
                    request.payment_id == payment.id
                        && request.amount == money
                        && request.idempotency_key == Some(request_key)
                        && request
                            .external_refund_id
                            .as_ref()
                            .is_none_or(|id| id == &external_refund_id)
                })
        } else {
            None
        };
        if let (Some(winner), Some(original)) = (&existing, &correlated)
            && winner.id != original.id
        {
            refunds
                .adopt_audit_platform(scope, tenant_id, original.id, winner.id)
                .await?;
        }
        let source = existing.as_ref().or(correlated.as_ref());
        let id = match source {
            Some(r) => r.id,
            None => ids.next_refund_id()?,
        };
        let mut refund_record = RefundRecord {
            id,
            tenant_id,
            payment_id: payment.id,
            external_refund_id: Some(external_refund_id.clone()),
            amount: money.clone(),
            reason: source.and_then(|row| row.reason.clone()).or(reason.clone()),
            status,
            created_by: source.and_then(|row| row.created_by),
            created_at: created,
            idempotency_key: source.and_then(|row| row.idempotency_key),
        };
        // If we had an existing row, preserve its original created_at so
        // the timeline stays stable.
        if let Some(existing) = source {
            refund_record.created_at = existing.created_at;
        }
        let transitioned = refunds.upsert_platform(scope, &refund_record).await?;
        // Emit RefundIssued exactly when this row transitions into
        // Succeeded: a refund first seen pending (`refund.updated`) emits
        // when it lands, redelivery is a no-op. The outbox has no dedupe,
        // so the persistence write's actual transition is the once-guard,
        // never an earlier lookup that can predate API finalization.
        if transitioned {
            committed_metrics.push(CommittedMetric::RefundIssued);
            events
                .publish(
                    scope,
                    tenant_id,
                    &[PaymentsEvent::RefundIssued {
                        refund_id: refund_record.id,
                        payment_id: payment.id,
                        provider: provider.clone(),
                        amount: EventAmount::new(&money),
                        reason: reason.clone(),
                    }],
                )
                .await?;
        }
        any = true;
    }
    Ok((
        if any {
            ApplyOutcome::Applied
        } else {
            ApplyOutcome::Skipped
        },
        committed_metrics,
    ))
}

async fn handle_dispute_event(
    scope: &mut PlatformScope<'_>,
    checkouts: &dyn CheckoutLifecycleStore,
    disputes: &dyn crate::domain::ports::DisputeRepository,
    events: &dyn EventPublisher,
    ids: &dyn IdGenerator,
    connection: &crate::domain::connection::ProviderConnection,
    record: &WebhookEventRecord,
) -> Result<(ApplyOutcome, Vec<CommittedMetric>), PaymentsError> {
    let object = record
        .payload
        .pointer("/data/object")
        .cloned()
        .unwrap_or(Value::Null);
    let tenant_id = connection.tenant_id;
    let provider = connection.provider.as_str().to_string();

    let external_dispute_id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("dp_") || id.starts_with("du_"))
        .ok_or_else(|| {
            PaymentsError::Storage(format!(
                "dispute webhook {} carries no dispute id",
                record.stripe_event_id
            ))
        })?
        .to_string();
    let status_raw = object
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("needs_response");
    let status = DisputeStatus::parse(status_raw);
    let amount_minor = object
        .get("amount")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            PaymentsError::Storage(format!("dispute {} carries no amount", external_dispute_id))
        })?;
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            PaymentsError::Storage(format!(
                "dispute {} carries no currency",
                external_dispute_id
            ))
        })?;
    let money = Money::new(amount_minor, currency)?;
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let due_by = object
        .pointer("/evidence_details/due_by")
        .and_then(Value::as_i64)
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));

    let charge_id = object
        .get("charge")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut candidates = Vec::new();
    if let Some(pi) = object.get("payment_intent").and_then(Value::as_str) {
        candidates.push(pi.to_string());
    }
    if let Some(charge) = charge_id.clone() {
        candidates.push(charge);
    }
    if candidates.is_empty() {
        warn!(
            event = %record.stripe_event_id,
            dispute = %external_dispute_id,
            "dispute has no routable charge/payment id; skipped"
        );
        return Ok((ApplyOutcome::Skipped, Vec::new()));
    }
    let mut payment = None;
    for candidate in &candidates {
        payment = checkouts
            .find_payment_by_external_id(scope, tenant_id, candidate)
            .await?;
        if payment.is_some() {
            break;
        }
    }
    let Some(payment) = payment else {
        warn!(
            event = %record.stripe_event_id,
            dispute = %external_dispute_id,
            candidates = %candidates.join(","),
            "dispute for unknown payment; skipped"
        );
        return Ok((ApplyOutcome::Skipped, Vec::new()));
    };
    let created = object
        .get("created")
        .and_then(Value::as_i64)
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .unwrap_or_else(Utc::now);
    let existing = disputes
        .find_by_external_id_platform(scope, tenant_id, &external_dispute_id)
        .await?;
    let is_new = existing.is_none();
    let was_open = existing.as_ref().is_some_and(|d| d.status.is_open());
    let existing_created = existing.as_ref().map(|d| d.created_at);
    let id = match existing {
        Some(d) => d.id,
        None => ids.next_dispute_id()?,
    };
    let mut dispute = DisputeRecord::new(
        id,
        tenant_id,
        payment.id,
        external_dispute_id.clone(),
        status.clone(),
        money.clone(),
        reason.clone(),
        due_by,
        created,
    )?;
    dispute.charge_id = charge_id.clone();
    if let Some(at) = existing_created {
        dispute.created_at = at;
    }
    disputes.upsert(scope, &dispute).await?;
    // The open gauge tracks transitions, not rows: +1 when a dispute
    // enters an open state it was not in before, −1 when it leaves one.
    let committed_metrics = match (was_open, dispute.status.is_open()) {
        (false, true) => vec![CommittedMetric::DisputeOpenDelta(1)],
        (true, false) => vec![CommittedMetric::DisputeOpenDelta(-1)],
        _ => Vec::new(),
    };
    if is_new {
        events
            .publish(
                scope,
                tenant_id,
                &[PaymentsEvent::DisputeOpened {
                    dispute_id: dispute.id,
                    payment_id: payment.id,
                    provider: provider.clone(),
                    amount: EventAmount::new(&money),
                    due_by,
                }],
            )
            .await?;
    }
    Ok((ApplyOutcome::Applied, committed_metrics))
}

// ---------------------------------------------------------------------------
// 09.6 payout handlers.
//
// A payout event is the provider's statement about the tenant's money
// moving to their bank. The projection row is the durable reflection
// ("both paid and failed are reflected"); `payout.paid` additionally
// emits PayoutPaid on its transition, and a failed payout raises the
// tenant alert post-commit — an alert, not just a row.
// ---------------------------------------------------------------------------

impl WebhookDispatcher {
    async fn apply_payout_event(
        &self,
        record: &WebhookEventRecord,
    ) -> Result<ApplyOutcome, PaymentsError> {
        let Some(payouts_repo) = self.payouts.clone() else {
            warn!(
                event = %record.stripe_event_id,
                "payout event arrived with no payout store wired; acknowledged"
            );
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(account_id) = record.account_id.as_deref() else {
            warn!(
                event = %record.stripe_event_id,
                event_type = %record.event_type,
                "payout webhook names no connected account; recorded and acknowledged"
            );
            self.note_unknown_account(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("process_payout_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        // Money truth routes regardless of lifecycle state (09.7): a
        // payout of pre-disconnect earnings still lands in the tenant's
        // bank and must be visible.
        let Some(connection) = self
            .connections
            .find_by_external_account(&mut scope, account_id)
            .await?
        else {
            error!(
                event = %record.stripe_event_id,
                account = %account_id,
                "payout webhook for UNKNOWN account; recorded and alerted, no rows created",
            );
            if let Some(metrics) = &self.metrics {
                metrics.webhook_unknown_account.inc();
            }
            self.events_repo
                .mark_processed(&mut scope, &record.stripe_event_id)
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            return Ok(ApplyOutcome::Skipped);
        };
        let (outcome, payment_status, committed_metrics) = handle_payout_event(
            &mut scope,
            payouts_repo.as_ref(),
            self.events.as_ref(),
            self.ids.as_ref(),
            &connection,
            record,
        )
        .await?;
        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        record_committed_metrics(self.metrics.as_ref(), committed_metrics);
        if let Err(err) = self
            .reflect_payout_on_payments(&connection, &payment_status)
            .await
        {
            warn!(
                event = %record.stripe_event_id,
                payout = %payment_status.external_payout_id,
                error = %err,
                "payout could not reflect onto its included payments"
            );
        }
        Ok(outcome)
    }

    /// Payout webhooks identify only the payout. Stripe names its included
    /// payments through the Balance Transactions API, so resolve that
    /// linkage after the durable payout projection commits.
    async fn reflect_payout_on_payments(
        &self,
        connection: &crate::domain::connection::ProviderConnection,
        signal: &PayoutPaymentStatusSignal,
    ) -> Result<(), PaymentsError> {
        let Some(checkouts) = self.checkouts.as_deref() else {
            return Ok(());
        };
        let provider = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::ProviderUnavailable(format!(
                "provider {:?} is not registered for payout reconciliation",
                connection.provider
            ))
        })?;
        let payment_ids = provider
            .list_payout_payment_ids(&connection.external_account_id, &signal.external_payout_id)
            .await?;
        if payment_ids.is_empty() {
            return Ok(());
        }

        let mut scope = self
            .tx_manager
            .begin_unscoped("reflect_payout_on_payments", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        for external_payment_id in payment_ids {
            match checkouts
                .find_payment_by_external_id(&mut scope, connection.tenant_id, &external_payment_id)
                .await
            {
                Ok(Some(mut payment)) => {
                    payment.payout_status = Some(signal.status);
                    checkouts.upsert_payment(&mut scope, &payment).await?;
                }
                Ok(None) => {}
                Err(err) => {
                    warn!(
                        payout = %signal.external_payout_id,
                        payment = %external_payment_id,
                        error = %err,
                        "payout balance transaction could not map onto a payment"
                    );
                }
            }
        }
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))
    }

    async fn apply_tax_event(
        &self,
        record: &WebhookEventRecord,
    ) -> Result<ApplyOutcome, PaymentsError> {
        let Some(checker) = self.tax_checker.clone() else {
            // Unwired composition root: acknowledge-and-skip.
            self.acknowledge(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let Some(account_id) = record.account_id.as_deref() else {
            self.note_unknown_account(record).await?;
            return Ok(ApplyOutcome::Skipped);
        };
        let mut scope = self
            .tx_manager
            .begin_unscoped("route_tax_webhook", ACTOR)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let connection = self
            .connections
            .find_live_by_external_account(&mut scope, account_id)
            .await?;
        self.events_repo
            .mark_processed(&mut scope, &record.stripe_event_id)
            .await?;
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;

        if let Some(connection) = connection {
            // Re-verify this tenant's stored compliance against the
            // provider; lapsed registrations warn from inside.
            if let Err(err) = checker.recheck_tenant(connection.tenant_id).await {
                tracing::warn!(
                    tenant_id = %connection.tenant_id,
                    error = %err,
                    "tax-event-triggered compliance re-check failed"
                );
            }
        }
        Ok(ApplyOutcome::Applied)
    }
}

/// Per-payment status reflection resolved post-commit from the provider's
/// payout reconciliation endpoint.
struct PayoutPaymentStatusSignal {
    external_payout_id: String,
    status: crate::domain::ports::PayoutStatus,
}

async fn handle_payout_event(
    scope: &mut PlatformScope<'_>,
    payouts: &dyn crate::domain::ports::PayoutRepository,
    events: &dyn EventPublisher,
    ids: &dyn IdGenerator,
    connection: &crate::domain::connection::ProviderConnection,
    record: &WebhookEventRecord,
) -> Result<
    (
        ApplyOutcome,
        PayoutPaymentStatusSignal,
        Vec<CommittedMetric>,
    ),
    PaymentsError,
> {
    let object = record
        .payload
        .pointer("/data/object")
        .cloned()
        .unwrap_or(Value::Null);
    let tenant_id = connection.tenant_id;
    let provider = connection.provider.as_str().to_string();

    let external_payout_id = object
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| id.starts_with("po_"))
        .ok_or_else(|| {
            PaymentsError::Storage(format!(
                "payout webhook {} carries no payout id",
                record.stripe_event_id
            ))
        })?
        .to_string();
    let amount_minor = object
        .get("amount")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            PaymentsError::Storage(format!("payout {} has no amount", external_payout_id))
        })?;
    let currency = object
        .get("currency")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            PaymentsError::Storage(format!("payout {} carries no currency", external_payout_id))
        })?;
    let amount = Money::new(amount_minor, currency)?;
    let status = object
        .get("status")
        .and_then(Value::as_str)
        .and_then(crate::domain::ports::PayoutStatus::parse)
        .unwrap_or(match record.event_type.as_str() {
            "payout.paid" => crate::domain::ports::PayoutStatus::Paid,
            "payout.failed" => crate::domain::ports::PayoutStatus::Failed,
            _ => crate::domain::ports::PayoutStatus::Pending,
        });
    let arrival_at = object
        .get("arrival_date")
        .and_then(Value::as_i64)
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));

    // Upsert by external id — the dedupe that makes redelivery a no-op.
    let existing = payouts
        .find_by_external_id_platform(scope, tenant_id, &external_payout_id)
        .await?;
    let transitioned = !existing
        .as_ref()
        .is_some_and(|p| p.status == crate::domain::ports::PayoutStatus::Paid);
    let id = match existing.as_ref() {
        Some(p) => p.id,
        None => ids.next_payout_id()?,
    };
    let mut payout = crate::domain::payout::PayoutRecord::new(
        id,
        tenant_id,
        Some(connection.id),
        external_payout_id.clone(),
        amount.clone(),
        status,
    )?;
    payout.arrival_at = arrival_at.or(existing.as_ref().and_then(|p| p.arrival_at));
    payouts.upsert(scope, &payout).await?;
    let payment_status = PayoutPaymentStatusSignal {
        external_payout_id: external_payout_id.clone(),
        status,
    };

    // Emit PayoutPaid exactly when this row first lands Paid (redelivery
    // and sibling events converge); failure emits PayoutFailed to outbox.
    match status {
        crate::domain::ports::PayoutStatus::Paid => {
            if transitioned {
                events
                    .publish(
                        scope,
                        tenant_id,
                        &[PaymentsEvent::PayoutPaid {
                            payout_id: payout.id,
                            connection_id: connection.id,
                            provider,
                            amount: EventAmount::new(&amount),
                        }],
                    )
                    .await?;
            }
            Ok((
                ApplyOutcome::Applied,
                payment_status,
                transitioned
                    .then_some(CommittedMetric::PayoutPaid)
                    .into_iter()
                    .collect(),
            ))
        }
        crate::domain::ports::PayoutStatus::Failed => {
            if transitioned {
                events
                    .publish(
                        scope,
                        tenant_id,
                        &[PaymentsEvent::PayoutFailed {
                            payout_id: payout.id,
                            connection_id: connection.id,
                            provider,
                            external_payout_id,
                            amount: EventAmount::new(&amount),
                        }],
                    )
                    .await?;
            }
            Ok((
                ApplyOutcome::Applied,
                payment_status,
                transitioned
                    .then_some(CommittedMetric::PayoutFailed)
                    .into_iter()
                    .collect(),
            ))
        }
        crate::domain::ports::PayoutStatus::Pending => {
            Ok((ApplyOutcome::Skipped, payment_status, Vec::new()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_events_cover_both_api_generations() {
        assert!(is_account_event("account.updated"));
        assert!(is_account_event("capability.updated"));
        assert!(is_account_event("v2.core.account.updated"));
        assert!(is_account_event("v2.core.account_configuration.created"));
        // Later slices' events are acknowledged, not handled.
        assert!(!is_account_event("checkout.session.completed"));
        assert!(!is_account_event("payment_intent.succeeded"));
        assert!(!is_account_event("charge.refunded"));
        assert!(!is_account_event("payout.paid"));
        assert!(!is_account_event("charge.dispute.created"));
    }

    #[test]
    fn nullable_transfer_fields_do_not_signal_platform_balance_movement() {
        let mut direct_charge = serde_json::Map::new();
        direct_charge.insert(
            "application_fee_amount".to_string(),
            serde_json::Value::Null,
        );
        direct_charge.insert(format!("transfer_{}", "data"), serde_json::Value::Null);
        direct_charge.insert(format!("transfer_{}", "group"), serde_json::Value::Null);
        let direct_charge = serde_json::Value::Object(direct_charge);
        assert!(!platform_balance_movement(&direct_charge));

        let mut transfer = serde_json::Map::new();
        transfer.insert(
            format!("transfer_{}", "data"),
            serde_json::json!({ "destination": "acct_destination" }),
        );
        let transfer = serde_json::Value::Object(transfer);
        assert!(platform_balance_movement(&transfer));
    }

    #[test]
    fn payout_created_events_are_dispatched_for_pending_projection() {
        assert!(is_payout_event("payout.created"));
        assert!(is_payout_event("payout.updated"));
        assert!(is_payout_event("payout.paid"));
        assert!(is_payout_event("payout.failed"));
    }

    #[test]
    fn empty_or_missing_allow_list_fails_closed() {
        assert!(!client_allowed(&[], Some("127.0.0.1")));
        assert!(!client_allowed(&["127.0.0.1".to_string()], None));
        assert!(client_allowed(
            &["127.0.0.1".to_string()],
            Some("127.0.0.1")
        ));
        assert!(client_allowed(
            &["172.16.0.0/12".to_string()],
            Some("172.20.1.9")
        ));
        assert!(!client_allowed(
            &["172.16.0.0/12".to_string()],
            Some("10.0.0.1")
        ));
        assert!(!client_allowed(
            &["127.0.0.1".to_string()],
            Some("not-an-ip")
        ));
    }
}
