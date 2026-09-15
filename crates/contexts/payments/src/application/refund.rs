//! The refund use case: 09.5's most dangerous operation.
//!
//! Irreversible, arithmetic-sensitive and currency-sensitive — the UI
//! shows the exact amount that actually moved (never a bare 204), and
//! the API gates it behind both `payments.refund` (the permission
//! extractor, enforced at the HTTP layer) and an explicit audit reason.
//! Idempotency is enforced locally **and** passed to Stripe, so a
//! retried request after a lost response never issues a second refund.
//!
//! Order: validate reason → load payment → replay/re-drive an existing
//! key → compute remaining → persist pending → call provider on the
//! **connected account** → finalize (external id + status + outbox) →
//! audit. A provider rejection marks the row failed so a retried key can
//! re-drive; the provider-side idempotency key keeps the re-drive safe.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::money::Money;
use crate::domain::payment::{PaymentId, RefundId, RefundRecord, RefundStatus};
use crate::domain::ports::{
    CheckoutRepository, IdGenerator, PaymentRepository, ProviderConnectionRepository,
    ProviderRefundCommand, RefundRepository,
};

use super::registry::ProviderRegistry;

/// What the handler parsed from the refund request body.
#[derive(Debug, Clone)]
pub struct RefundCommand {
    pub tenant_id: TenantId,
    pub payment_id: PaymentId,
    /// `None` → full remaining amount (the Dashboard's "refund all"
    /// path), `Some(money)` → partial. When `Some`, the currency must
    /// match the payment's currency and the minor units must not exceed
    /// the remaining refundable amount.
    pub amount: Option<Money>,
    /// Why the tenant is issuing this refund — required, audited, and
    /// shown in the timeline. Empty is rejected.
    pub reason: String,
    /// Optional free-form note (not the audit reason), stored as the
    /// refund's `reason` extension when present? The spec's request has
    /// `reason` and `note`; we store `reason` on the refund row and put
    /// `note` in the audit `after` payload so support sees both.
    pub note: Option<String>,
    pub idempotency_key: Uuid,
    pub actor: UserId,
    pub ip: Option<IpAddr>,
}

pub struct IssueRefund {
    tx_manager: TxManager,
    registry: Arc<ProviderRegistry>,
    payments: Arc<dyn PaymentRepository>,
    refunds: Arc<dyn RefundRepository>,
    connections: Arc<dyn ProviderConnectionRepository>,
    checkouts: Arc<dyn CheckoutRepository>,
    events: Arc<dyn EventPublisher>,
    audit: Arc<dyn AuditWriter>,
    ids: Arc<dyn IdGenerator>,
}

impl IssueRefund {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        registry: Arc<ProviderRegistry>,
        payments: Arc<dyn PaymentRepository>,
        refunds: Arc<dyn RefundRepository>,
        connections: Arc<dyn ProviderConnectionRepository>,
        checkouts: Arc<dyn CheckoutRepository>,
        events: Arc<dyn EventPublisher>,
        audit: Arc<dyn AuditWriter>,
        ids: Arc<dyn IdGenerator>,
    ) -> Self {
        Self {
            tx_manager,
            registry,
            payments,
            refunds,
            connections,
            checkouts,
            events,
            audit,
            ids,
        }
    }

    /// Issue (or replay, or re-drive) a refund. Returns the refund record
    /// and whether this call replayed an already-finalized refund (the
    /// handler answers 200 for a replay, 201 for a fresh issue).
    pub async fn execute(
        &self,
        command: RefundCommand,
    ) -> Result<(RefundRecord, bool), PaymentsError> {
        let reason = command.reason.trim().to_string();
        if reason.is_empty() {
            return Err(PaymentsError::InvalidConnection(
                "refund reason is required".to_string(),
            ));
        }
        if reason.len() > 500 {
            return Err(PaymentsError::InvalidConnection(
                "refund reason exceeds 500 characters".to_string(),
            ));
        }

        // Load the payment, tenant-scoped (RLS ensures cross-tenant probes
        // return None, never someone else's row).
        let payment = {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id.as_i64())
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            let payment = self
                .payments
                .find_by_id(&mut scope, command.tenant_id, command.payment_id)
                .await?
                .ok_or_else(|| {
                    PaymentsError::InvalidConnection(format!(
                        "payment {} not found",
                        command.payment_id
                    ))
                })?;
            let _ = scope.rollback().await;
            payment
        };

        // Refunds only against succeeded payments — a failed payment has no
        // captured funds to return.
        if payment.status != crate::domain::PaymentStatus::Succeeded {
            return Err(PaymentsError::InvalidConnection(format!(
                "payment {} is {} and cannot be refunded",
                payment.id,
                payment.status.as_str()
            )));
        }

        // Replay BEFORE any remaining-amount guard: a retried key whose
        // refund already succeeded must answer with that refund, not with
        // an over-refund error computed against the refunded total.
        let existing = {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id.as_i64())
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            let existing = self
                .refunds
                .find_by_idempotency_key(&mut scope, command.tenant_id, command.idempotency_key)
                .await?;
            let _ = scope.rollback().await;
            existing
        };
        if let Some(existing) = existing {
            if existing.payment_id != payment.id {
                return Err(PaymentsError::IdempotencyConflict(
                    "Idempotency-Key already used for another payment".to_string(),
                ));
            }
            if !payload_matches(&existing, &command.amount, &reason) {
                return Err(PaymentsError::IdempotencyConflict(
                    "Idempotency-Key reused with different amount or reason".to_string(),
                ));
            }
            match existing.status {
                RefundStatus::Succeeded => return Ok((existing, true)),
                // Pending or failed: the provider call either never
                // happened (crash between insert and call) or was
                // rejected. Re-drive it — the provider-side idempotency
                // key guarantees a refund that already exists at the
                // provider is replayed, never duplicated.
                RefundStatus::Pending | RefundStatus::Failed => {
                    let connection = self.resolve_connection(command.tenant_id, &payment).await?;
                    let finalized = self
                        .drive_and_finalize(&command, &payment, &connection, existing)
                        .await?;
                    return Ok((finalized, false));
                }
            }
        }

        // Compute remaining refundable amount: total minus already-succeeded
        // refunds (in minor units, same currency). Partial refunds that sum
        // to the total are the honest path; the next unit over is the
        // error the spec names explicitly.
        let refunded_minor = {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id.as_i64())
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            let sum = self
                .refunds
                .sum_succeeded_for_payment(&mut scope, command.tenant_id, payment.id)
                .await?;
            let _ = scope.rollback().await;
            sum
        };
        let remaining_minor = payment
            .amount
            .amount_minor
            .checked_sub(refunded_minor)
            .ok_or_else(|| {
                PaymentsError::Storage("refund sum exceeds payment total".to_string())
            })?;
        if remaining_minor <= 0 {
            return Err(PaymentsError::RefundExceedsRemaining {
                requested_minor: command
                    .amount
                    .as_ref()
                    .map_or(remaining_minor, |m| m.amount_minor),
                requested_currency: command
                    .amount
                    .as_ref()
                    .map_or(payment.amount.currency.as_str().to_string(), |m| {
                        m.currency.as_str().to_string()
                    }),
                remaining_minor: 0,
                remaining_currency: payment.amount.currency.as_str().to_string(),
            });
        }
        let remaining = Money::new(remaining_minor, payment.amount.currency.as_str())?;

        let requested = match &command.amount {
            None => remaining.clone(),
            Some(money) => {
                if money.currency != payment.amount.currency {
                    return Err(PaymentsError::InvalidMoney(format!(
                        "refund currency {} does not match payment currency {}",
                        money.currency.as_str(),
                        payment.amount.currency.as_str()
                    )));
                }
                if money.is_zero() {
                    return Err(PaymentsError::InvalidMoney(
                        "refund amount must be positive".to_string(),
                    ));
                }
                if money.amount_minor > remaining_minor {
                    return Err(PaymentsError::RefundExceedsRemaining {
                        requested_minor: money.amount_minor,
                        requested_currency: money.currency.as_str().to_string(),
                        remaining_minor,
                        remaining_currency: payment.amount.currency.as_str().to_string(),
                    });
                }
                money.clone()
            }
        };

        let refund_id = self.ids.next_refund_id()?;
        let now = Utc::now();
        let mut refund = RefundRecord::against(
            refund_id,
            &payment,
            requested.amount_minor,
            Some(reason.clone()),
            Some(command.actor),
            now,
        )?;
        refund.idempotency_key = Some(command.idempotency_key);

        // Persist Pending before the provider call — the idempotency key
        // now has an owner even if the HTTP response is lost.
        let mut replayed_winner = None;
        {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id.as_i64())
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            match self.refunds.insert(&mut scope, &refund).await {
                Ok(()) => {
                    scope
                        .commit()
                        .await
                        .map_err(|err| PaymentsError::Storage(err.to_string()))?;
                }
                Err(PaymentsError::IdempotencyConflict(detail)) => {
                    let _ = scope.rollback().await;
                    // Adopt the winner of a concurrent same-key insert —
                    // after the same payload validation the replay path
                    // applies. A different payload under the same key is a
                    // conflict, not a success.
                    let mut scope2 = self
                        .tx_manager
                        .begin_for(command.tenant_id.as_i64())
                        .await
                        .map_err(|err| PaymentsError::Storage(err.to_string()))?;
                    let winner = self
                        .refunds
                        .find_by_idempotency_key(
                            &mut scope2,
                            command.tenant_id,
                            command.idempotency_key,
                        )
                        .await?
                        .ok_or(PaymentsError::IdempotencyConflict(detail))?;
                    let _ = scope2.rollback().await;
                    if winner.payment_id != payment.id
                        || !payload_matches(&winner, &command.amount, &reason)
                    {
                        return Err(PaymentsError::IdempotencyConflict(
                            "Idempotency-Key reused with different amount or reason".to_string(),
                        ));
                    }
                    replayed_winner = Some(winner);
                }
                Err(err) => {
                    let _ = scope.rollback().await;
                    return Err(err);
                }
            }
        }
        if let Some(winner) = replayed_winner {
            // The concurrent winner may itself still be mid-flight; its
            // record is the answer either way (a pending record for an
            // in-flight call, succeeded for a finished one).
            return Ok((winner, true));
        }

        let connection = self.resolve_connection(command.tenant_id, &payment).await?;
        let finalized = self
            .drive_and_finalize(&command, &payment, &connection, refund)
            .await?;
        Ok((finalized, false))
    }

    /// The connected account this payment's money moved through — the
    /// `Stripe-Account` the refund is executed on. The normal path is
    /// payment → checkout → connection; payments without a checkout (direct
    /// PaymentIntent webhooks) fall back to the tenant's live connection.
    async fn resolve_connection(
        &self,
        tenant_id: TenantId,
        payment: &crate::domain::PaymentRecord,
    ) -> Result<crate::domain::ProviderConnection, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let connection = if let Some(checkout_id) = payment.checkout_id {
            let checkout = self
                .checkouts
                .find_by_id(&mut scope, tenant_id, checkout_id)
                .await?
                .ok_or_else(|| {
                    PaymentsError::Storage(format!("checkout {checkout_id} not found"))
                })?;
            self.connections
                .find_by_id(&mut scope, tenant_id, checkout.connection_id)
                .await?
                .ok_or_else(|| {
                    PaymentsError::Storage("connection for checkout not found".to_string())
                })?
        } else {
            let connections = self.connections.list(&mut scope, tenant_id).await?;
            connections
                .into_iter()
                .find(|c| c.disconnected_at.is_none())
                .ok_or_else(|| {
                    PaymentsError::ConnectionNotActive("no live connection for refund".to_string())
                })?
        };
        let _ = scope.rollback().await;

        // Guard: disconnected connections cannot move money.
        if connection.disconnected_at.is_some()
            || connection.status == crate::domain::ConnectionStatus::Disconnected
            || connection.status == crate::domain::ConnectionStatus::Rejected
        {
            return Err(PaymentsError::ConnectionNotActive(format!(
                "connection is {}",
                connection.status.as_str()
            )));
        }
        Ok(connection)
    }

    /// Call the provider on the connected account, then persist its answer.
    /// The refund is the tenant's money moving; Sanvi never holds it. The
    /// provider's idempotency key is our `idempotency_key`, so a re-drive of
    /// a key the provider already served replays that refund instead of
    /// issuing a second one. A provider rejection marks the local row
    /// failed — the read model must never show a pending refund that does
    /// not exist at the provider, and a retried key stays re-drivable.
    async fn drive_and_finalize(
        &self,
        command: &RefundCommand,
        payment: &crate::domain::PaymentRecord,
        connection: &crate::domain::ProviderConnection,
        mut refund: RefundRecord,
    ) -> Result<RefundRecord, PaymentsError> {
        let adapter = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available",
                connection.provider.as_str()
            ))
        })?;

        let provider_refund = match adapter
            .refund(&ProviderRefundCommand {
                external_account_id: connection.external_account_id.clone(),
                external_payment_id: payment.external_payment_id.clone(),
                refund: refund.clone(),
                idempotency_key: command.idempotency_key,
            })
            .await
        {
            Ok(provider_refund) => provider_refund,
            Err(err) => {
                self.mark_failed(command.tenant_id, refund.id).await;
                return Err(err);
            }
        };

        refund.external_refund_id = Some(provider_refund.external_refund_id.clone());
        refund.status = provider_refund.status;
        let finalized = self.finalize(command, payment, connection, refund).await?;

        // Audit: actor, amount, currency, reason, plus the note when present.
        self.audit_refund(command, payment, &finalized, connection)
            .await;
        Ok(finalized)
    }

    /// Persist the provider's answer in one transaction with the
    /// `RefundIssued` outbox event. If the refund webhook won the race and
    /// already created a row for this provider refund id, that row wins:
    /// ours is deleted and returned (the webhook path already finalized
    /// and emitted for it).
    async fn finalize(
        &self,
        command: &RefundCommand,
        payment: &crate::domain::PaymentRecord,
        connection: &crate::domain::ProviderConnection,
        refund: RefundRecord,
    ) -> Result<RefundRecord, PaymentsError> {
        let external_refund_id = refund
            .external_refund_id
            .clone()
            .ok_or_else(|| PaymentsError::Storage("refund has no provider id".to_string()))?;
        let mut scope = self
            .tx_manager
            .begin_for(command.tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let winner = match self
            .refunds
            .find_by_external_id(&mut scope, command.tenant_id, &external_refund_id)
            .await?
        {
            Some(existing) if existing.id != refund.id => Some(existing),
            _ => None,
        };
        if let Some(mut winner) = winner {
            self.refunds
                .delete_by_id(&mut scope, command.tenant_id, refund.id)
                .await?;
            // The webhook-created row owns the provider refund, but the
            // local row owns the caller's key. Move that ownership before
            // committing so every later retry still replays this refund.
            self.refunds
                .claim_idempotency_key(
                    &mut scope,
                    command.tenant_id,
                    winner.id,
                    command.idempotency_key,
                )
                .await?;
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            winner.idempotency_key = Some(command.idempotency_key);
            return Ok(winner);
        }
        let transitioned = self
            .refunds
            .try_finalize(&mut scope, command.tenant_id, &refund)
            .await?;
        if transitioned && refund.status == RefundStatus::Succeeded {
            // The outbox has no dedupe — the row's pending/failed →
            // succeeded transition is the once-guard for RefundIssued.
            self.events
                .publish_scoped(
                    &mut scope,
                    command.tenant_id,
                    &[PaymentsEvent::RefundIssued {
                        refund_id: refund.id,
                        payment_id: payment.id,
                        provider: connection.provider.as_str().to_string(),
                        amount: crate::domain::events::EventAmount::new(&refund.amount),
                        reason: refund.reason.clone(),
                    }],
                )
                .await?;
        }
        scope
            .commit()
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        Ok(refund)
    }

    /// Best-effort failure marking: a storage failure here is logged and
    /// swamped by the provider error the caller already holds — the row
    /// stays pending and the next same-key retry re-drives it.
    async fn mark_failed(&self, tenant_id: TenantId, refund_id: RefundId) {
        let Ok(mut scope) = self.tx_manager.begin_for(tenant_id.as_i64()).await else {
            tracing::error!(refund_id = %refund_id, "failed to open scope to mark refund failed");
            return;
        };
        if let Err(err) = self
            .refunds
            .mark_failed(&mut scope, tenant_id, refund_id)
            .await
        {
            tracing::error!(refund_id = %refund_id, error = %err, "failed to mark refund failed");
        }
        if let Err(err) = scope.commit().await {
            tracing::error!(refund_id = %refund_id, error = %err, "failed to commit refund failure");
        }
    }

    async fn audit_refund(
        &self,
        command: &RefundCommand,
        payment: &crate::domain::PaymentRecord,
        refund: &RefundRecord,
        connection: &crate::domain::ProviderConnection,
    ) {
        let Ok(mut scope) = self
            .tx_manager
            .begin_unscoped("audit_payments_refund_issued", "payments")
            .await
        else {
            tracing::error!(refund_id = %refund.id, "failed to open audit scope for refund");
            return;
        };
        let detail = serde_json::json!({
            "payment_id": payment.id,
            "external_payment_id": payment.external_payment_id,
            "refund_id": refund.id,
            "external_refund_id": refund.external_refund_id,
            "amount_minor": refund.amount.amount_minor,
            "currency": refund.amount.currency.as_str(),
            "reason": refund.reason,
            "note": command.note,
            "provider": connection.provider.as_str(),
            "external_account_id": connection.external_account_id,
        });
        if let Err(err) = self
            .audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: Utc::now(),
                    actor_type: ActorType::User,
                    actor_id: Some(command.actor),
                    tenant_id: Some(command.tenant_id),
                    action: "payments.refund_issued".to_string(),
                    resource_type: Some("payments.refund".to_string()),
                    resource_id: Some(refund.id.to_string()),
                    before: None,
                    after: Some(detail),
                    request_id: None,
                    ip: command.ip,
                    user_agent: None,
                    on_behalf_of: None,
                },
            )
            .await
        {
            tracing::error!(refund_id = %refund.id, error = %err, "failed to audit refund");
        }
        if let Err(err) = scope.commit().await {
            tracing::error!(refund_id = %refund.id, error = %err, "failed to commit refund audit");
        }
    }
}

/// Whether a retried request's payload describes the same refund as the
/// row its Idempotency-Key owns: same payment, same reason, and — when the
/// retry names an explicit amount — the exact amount. A `null` amount ("all
/// of it") replays whatever the key owns, because that is what "all of it"
/// resolved to when the key was first spent.
fn payload_matches(existing: &RefundRecord, requested: &Option<Money>, reason: &str) -> bool {
    existing.reason.as_deref() == Some(reason)
        && requested.as_ref().is_none_or(|m| *m == existing.amount)
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    fn record(amount_minor: i64, reason: &str) -> RefundRecord {
        RefundRecord {
            id: RefundId::try_from(101).unwrap(),
            tenant_id: TenantId::try_from(102).unwrap(),
            payment_id: PaymentId::try_from(103).unwrap(),
            external_refund_id: Some("re_x".to_string()),
            amount: Money::new(amount_minor, "JPY").unwrap(),
            reason: Some(reason.to_string()),
            status: RefundStatus::Succeeded,
            created_by: None,
            created_at: Utc::now(),
            idempotency_key: None,
        }
    }

    #[test]
    fn a_null_amount_replays_whatever_the_key_owns() {
        let existing = record(1500, "duplicate order");
        assert!(payload_matches(&existing, &None, "duplicate order"));
    }

    #[test]
    fn an_explicit_amount_must_match_exactly() {
        let existing = record(1500, "duplicate order");
        let same = Money::new(1500, "JPY").unwrap();
        let other = Money::new(500, "JPY").unwrap();
        assert!(payload_matches(&existing, &Some(same), "duplicate order"));
        assert!(!payload_matches(&existing, &Some(other), "duplicate order"));
    }

    #[test]
    fn a_changed_reason_conflicts() {
        let existing = record(1500, "duplicate order");
        assert!(!payload_matches(&existing, &None, "goodwill"));
    }
}
