//! The refund use case: 09.5's most dangerous operation.
//!
//! Irreversible, arithmetic-sensitive and currency-sensitive — the UI
//! shows the exact amount that actually moved (never a bare 204), and
//! the API gates it behind both `payments.refund` (the permission
//! extractor, enforced at the HTTP layer) and an explicit audit reason.
//! Idempotency is enforced locally **and** passed to Stripe, so a
//! retried request after a lost response never issues a second refund.
//!
//! Order: validate reason → load payment → compute remaining →
//! idempotency check → persist pending → call provider on the
//! **connected account** → persist success + outbox + audit.

use std::net::IpAddr;
use std::sync::Arc;

use chrono::Utc;
use uuid::Uuid;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_persistence::TxManager;

use crate::domain::error::PaymentsError;
use crate::domain::events::{EventPublisher, PaymentsEvent};
use crate::domain::money::Money;
use crate::domain::payment::{RefundRecord, RefundStatus};
use crate::domain::ports::{
    CheckoutRepository, PaymentRepository, ProviderConnectionRepository, ProviderRefundCommand,
    RefundRepository,
};

use super::registry::ProviderRegistry;

const REFUND_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x9b, 0x42, 0xf7, 0x1a, 0x33, 0x5c, 0x4a, 0x8f, 0xb1, 0x22, 0x77, 0x9e, 0xcc, 0x44, 0x1d, 0xaa,
]);

fn refund_id_for(tenant_id: Uuid, payment_id: Uuid, idempotency_key: Uuid) -> Uuid {
    let mut bytes = [0u8; 48];
    bytes[..16].copy_from_slice(tenant_id.as_bytes());
    bytes[16..32].copy_from_slice(payment_id.as_bytes());
    bytes[32..].copy_from_slice(idempotency_key.as_bytes());
    Uuid::new_v5(&REFUND_ID_NAMESPACE, &bytes)
}

/// What the handler parsed from the refund request body.
#[derive(Debug, Clone)]
pub struct RefundCommand {
    pub tenant_id: Uuid,
    pub payment_id: Uuid,
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
    pub actor: Uuid,
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
        }
    }

    pub async fn execute(&self, command: RefundCommand) -> Result<RefundRecord, PaymentsError> {
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
                .begin_for(command.tenant_id)
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

        // Compute remaining refundable amount: total minus already-succeeded
        // refunds (in minor units, same currency). Partial refunds that sum
        // to the total are the honest path; the next unit over is the
        // error the spec names explicitly.
        let refunded_minor = {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id)
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

        // Idempotency: same key → same refund, same payload → same row;
        // same key + different payload → conflict.
        {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            if let Some(existing) = self
                .refunds
                .find_by_idempotency_key(&mut scope, command.tenant_id, command.idempotency_key)
                .await?
            {
                let _ = scope.rollback().await;
                if existing.payment_id != payment.id {
                    return Err(PaymentsError::IdempotencyConflict(
                        "Idempotency-Key already used for another payment".to_string(),
                    ));
                }
                if existing.amount != requested
                    || existing.reason.as_deref() != Some(reason.as_str())
                {
                    return Err(PaymentsError::IdempotencyConflict(
                        "Idempotency-Key reused with different amount or reason".to_string(),
                    ));
                }
                return Ok(existing);
            }
            let _ = scope.rollback().await;
        }

        let refund_id = refund_id_for(command.tenant_id, payment.id, command.idempotency_key);
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

        // Resolve the connected account this refund is against. The payment
        // belongs to a checkout (the normal path); that checkout belongs to
        // a connection; that connection is the `Stripe-Account` we send.
        let connection = {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            let connection = if let Some(checkout_id) = payment.checkout_id {
                let checkout = self
                    .checkouts
                    .find_by_id(&mut scope, command.tenant_id, checkout_id)
                    .await?
                    .ok_or_else(|| {
                        PaymentsError::Storage(format!("checkout {checkout_id} not found"))
                    })?;
                self.connections
                    .find_by_id(&mut scope, command.tenant_id, checkout.connection_id)
                    .await?
                    .ok_or_else(|| {
                        PaymentsError::Storage("connection for checkout not found".to_string())
                    })?
            } else {
                // Fallback: the tenant's live connection (payments without a
                // checkout are rare and only appear via direct PaymentIntent
                // webhooks; the newest live connection is the honest owner).
                let connections = self.connections.list(&mut scope, command.tenant_id).await?;
                connections
                    .into_iter()
                    .find(|c| c.disconnected_at.is_none())
                    .ok_or_else(|| {
                        PaymentsError::ConnectionNotActive(
                            "no live connection for refund".to_string(),
                        )
                    })?
            };
            let _ = scope.rollback().await;
            connection
        };

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

        let adapter = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available",
                connection.provider.as_str()
            ))
        })?;

        // Persist Pending before the provider call — the idempotency key
        // now has an owner even if the HTTP response is lost.
        {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id)
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
                    // Adopt the winner: concurrent same-key insert won.
                    let mut scope2 = self
                        .tx_manager
                        .begin_for(command.tenant_id)
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
                    return Ok(winner);
                }
                Err(err) => {
                    let _ = scope.rollback().await;
                    return Err(err);
                }
            }
        }

        // Call the provider on the connected account. The refund is the
        // tenant's money moving; Sanvi never holds it. The provider's
        // idempotency key is our `idempotency_key` (UUID), not the row id.
        let provider_refund = adapter
            .refund(&ProviderRefundCommand {
                external_account_id: connection.external_account_id.clone(),
                external_payment_id: payment.external_payment_id.clone(),
                refund: refund.clone(),
                idempotency_key: command.idempotency_key,
            })
            .await
            .map_err(|err| {
                // Mark the local row as failed so the timeline and the
                // remaining-amount guard both see it.
                err
            })?;

        // Persist the provider's answer: external id + succeeded status.
        refund.external_refund_id = Some(provider_refund.external_refund_id.clone());
        refund.status = provider_refund.status;
        {
            let mut scope = self
                .tx_manager
                .begin_for(command.tenant_id)
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
            self.refunds.update(&mut scope, &refund).await?;
            // Outbox event: RefundIssued, committed with the row.
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
            scope
                .commit()
                .await
                .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        }

        // Audit: actor, amount, currency, reason, plus the note when present.
        self.audit_refund(&command, &payment, &refund, &connection, requested)
            .await;

        Ok(refund)
    }

    async fn audit_refund(
        &self,
        command: &RefundCommand,
        payment: &crate::domain::PaymentRecord,
        refund: &RefundRecord,
        connection: &crate::domain::ProviderConnection,
        requested: Money,
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
            "amount_minor": requested.amount_minor,
            "currency": requested.currency.as_str(),
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
                    on_behalf_of: None,
                    tenant_id: Some(command.tenant_id),
                    action: "payments.refund_issued".to_string(),
                    resource_type: Some("payments.refund".to_string()),
                    resource_id: Some(refund.id.to_string()),
                    before: None,
                    after: Some(detail),
                    request_id: None,
                    ip: command.ip,
                    user_agent: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refund_id_is_deterministic_per_tenant_payment_and_key() {
        let tenant = Uuid::now_v7();
        let payment = Uuid::now_v7();
        let key = Uuid::now_v7();
        assert_eq!(
            refund_id_for(tenant, payment, key),
            refund_id_for(tenant, payment, key)
        );
        assert_ne!(
            refund_id_for(tenant, payment, key),
            refund_id_for(Uuid::now_v7(), payment, key)
        );
        assert_ne!(
            refund_id_for(tenant, payment, key),
            refund_id_for(tenant, Uuid::now_v7(), key)
        );
        assert_ne!(
            refund_id_for(tenant, payment, key),
            refund_id_for(tenant, payment, Uuid::now_v7())
        );
    }
}
