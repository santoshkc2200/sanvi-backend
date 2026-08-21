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
use crate::domain::events::EventPublisher;
use crate::domain::ports::{
    ConnectionLifecycleStore, ConnectionNotifier, WebhookEventRepository, WebhookVerifierPort,
};
use crate::domain::webhook::{WebhookEventRecord, event_account_id, event_created};
use sanvi_persistence::TxManager;

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
/// (`account.updated`, `capability.updated`, `v2.core.account…`); checkout,
/// refund, dispute and payout handlers arrive with their slices — unknown
/// types are stored and acknowledged, never fatal.
pub struct WebhookDispatcher {
    tx_manager: TxManager,
    events_repo: Arc<dyn WebhookEventRepository>,
    connections: Arc<dyn ConnectionLifecycleStore>,
    registry: Arc<ProviderRegistry>,
    events: Arc<dyn EventPublisher>,
    notifier: Arc<dyn ConnectionNotifier>,
    metrics: Option<PaymentsMetrics>,
}

impl WebhookDispatcher {
    pub fn new(
        tx_manager: TxManager,
        events_repo: Arc<dyn WebhookEventRepository>,
        connections: Arc<dyn ConnectionLifecycleStore>,
        registry: Arc<ProviderRegistry>,
        events: Arc<dyn EventPublisher>,
        notifier: Arc<dyn ConnectionNotifier>,
    ) -> Self {
        Self {
            tx_manager,
            events_repo,
            connections,
            registry,
            events,
            notifier,
            metrics: None,
        }
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
    async fn apply(&self, record: &WebhookEventRecord) -> Result<ApplyOutcome, PaymentsError> {
        // Account/capability handlers only in this slice; everything else
        // (checkout, payment, refund, dispute, payout) lands with its own
        // slice and is acknowledged meanwhile.
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
            self.notifier.as_ref(),
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
        let mut scope = self
            .tx_manager
            .begin_unscoped("acknowledge_unroutable_connect_webhook", ACTOR)
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyOutcome {
    Applied,
    Skipped,
}

impl ApplyOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Applied => "processed",
            Self::Skipped => "skipped",
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
