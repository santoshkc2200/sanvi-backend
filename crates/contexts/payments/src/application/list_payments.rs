//! Payment read models: list, detail and timeline.
//!
//! The projection is kept by Connect webhooks (never fetched from Stripe
//! per request). List and detail are tenant-scoped via RLS and indexed
//! for every documented filter.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::dispute::DisputeRecord;
use crate::domain::error::PaymentsError;
use crate::domain::payment::{PaymentId, PaymentRecord, RefundRecord, RefundStatus};
use crate::domain::ports::{DisputeRepository, PaymentFilter, PaymentRepository, RefundRepository};
use crate::domain::timeline::{PaymentTimeline, TimelineEntry};

/// Filter as the handler parsed it from query params — strings still raw
/// so we can surface 400s with the field that failed.
#[derive(Debug, Clone)]
pub struct ListPaymentsCommand {
    pub tenant_id: TenantId,
    pub status: Option<String>,
    pub payout_status: Option<String>,
    pub currency: Option<String>,
    pub customer: Option<String>,
    pub date_from: Option<DateTime<Utc>>,
    pub date_to: Option<DateTime<Utc>>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// One page of payments as the API renders it.
#[derive(Debug, Clone)]
pub struct PaymentPageView {
    pub items: Vec<PaymentRecord>,
    pub next_cursor: Option<String>,
}

pub struct ListPayments {
    tx_manager: TxManager,
    payments: Arc<dyn PaymentRepository>,
}

impl ListPayments {
    pub fn new(tx_manager: TxManager, payments: Arc<dyn PaymentRepository>) -> Self {
        Self {
            tx_manager,
            payments,
        }
    }

    #[tracing::instrument(name = "payments.list_payments", skip_all, fields(tenant_id = %command.tenant_id))]
    pub async fn execute(
        &self,
        command: ListPaymentsCommand,
    ) -> Result<PaymentPageView, PaymentsError> {
        let mut filter = PaymentFilter::default();
        filter.customer = command
            .customer
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty());
        filter.date_from = command.date_from;
        filter.date_to = command.date_to;
        filter.cursor = command.cursor;
        filter.limit = command.limit;
        if let Some(raw) = command.status {
            let raw = raw.trim().to_ascii_lowercase();
            if !raw.is_empty() {
                // An undocumented status must 400 so the caller knows it
                // misspelled `succeeded` — an empty page would hide it.
                match crate::domain::PaymentStatus::parse(&raw) {
                    Some(status) => filter.status = Some(status),
                    None => {
                        return Err(PaymentsError::InvalidConnection(format!(
                            "unknown payment status {raw:?}"
                        )));
                    }
                }
            }
        }
        if let Some(raw) = command.payout_status {
            let raw = raw.trim().to_ascii_lowercase();
            if !raw.is_empty() {
                match crate::domain::PayoutStatus::parse(&raw) {
                    Some(status) => filter.payout_status = Some(status),
                    None => {
                        return Err(PaymentsError::InvalidConnection(format!(
                            "unknown payout status {raw:?}"
                        )));
                    }
                }
            }
        }
        if let Some(raw) = command.currency {
            let raw = raw.trim();
            if !raw.is_empty() {
                filter.currency = Some(crate::domain::Currency::parse(raw)?);
            }
        }
        let mut scope = self
            .tx_manager
            .begin_for(command.tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let page = self
            .payments
            .list(&mut scope, command.tenant_id, &filter)
            .await?;
        let _ = scope.rollback().await;
        Ok(PaymentPageView {
            items: page.items,
            next_cursor: page.next_cursor,
        })
    }
}

/// The detail response: payment + its refunds + its disputes + timeline.
///
/// The timeline is a first-class projection derived from stored events:
/// created, succeeded/failed, refunded, disputed, paid out. Support uses
/// it; the UI never recomputes.
#[derive(Debug, Clone)]
pub struct PaymentDetail {
    pub payment: PaymentRecord,
    pub refunds: Vec<RefundRecord>,
    pub disputes: Vec<DisputeRecord>,
    pub timeline: PaymentTimeline,
    /// The merchant's own reference for the originating checkout, when the
    /// payment has one.
    pub checkout_reference: Option<String>,
    /// The connected account the payment lives on — the anchor for
    /// Stripe Dashboard deep links. None when the connection is gone.
    pub account_external_id: Option<String>,
}

pub struct GetPaymentDetail {
    tx_manager: TxManager,
    payments: Arc<dyn PaymentRepository>,
    refunds: Arc<dyn RefundRepository>,
    disputes: Arc<dyn DisputeRepository>,
    checkouts: Arc<dyn crate::domain::CheckoutRepository>,
    connections: Arc<dyn crate::domain::ProviderConnectionRepository>,
}

impl GetPaymentDetail {
    pub fn new(
        tx_manager: TxManager,
        payments: Arc<dyn PaymentRepository>,
        refunds: Arc<dyn RefundRepository>,
        disputes: Arc<dyn DisputeRepository>,
        checkouts: Arc<dyn crate::domain::CheckoutRepository>,
        connections: Arc<dyn crate::domain::ProviderConnectionRepository>,
    ) -> Self {
        Self {
            tx_manager,
            payments,
            refunds,
            disputes,
            checkouts,
            connections,
        }
    }

    #[tracing::instrument(name = "payments.get_payment_detail", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        payment_id: PaymentId,
    ) -> Result<PaymentDetail, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let payment = self
            .payments
            .find_by_id(&mut scope, tenant_id, payment_id)
            .await?
            .ok_or_else(|| {
                PaymentsError::InvalidConnection(format!("payment {payment_id} not found"))
            })?;
        let refunds = self
            .refunds
            .list_for_payment(&mut scope, tenant_id, payment_id)
            .await?;
        let disputes = self
            .disputes
            .list_for_payment(&mut scope, tenant_id, payment_id)
            .await?;
        let checkout = if let Some(checkout_id) = payment.checkout_id {
            self.checkouts
                .find_by_id(&mut scope, tenant_id, checkout_id)
                .await?
        } else {
            None
        };
        let checkout_reference = checkout.as_ref().map(|c| c.reference.as_str().to_string());
        // The connected account the money moved through — the deep-link
        // anchor. Best effort: a missing checkout or connection only means
        // no link, never a failed detail view.
        let account_external_id = match &checkout {
            Some(checkout) => self
                .connections
                .find_by_id(&mut scope, tenant_id, checkout.connection_id)
                .await?
                .map(|c| c.external_account_id),
            None => {
                let connections = self.connections.list(&mut scope, tenant_id).await?;
                connections
                    .into_iter()
                    .find(|c| c.disconnected_at.is_none())
                    .map(|c| c.external_account_id)
            }
        };
        let _ = scope.rollback().await;

        let timeline = build_timeline(&payment, &refunds, &disputes);
        Ok(PaymentDetail {
            payment,
            refunds,
            disputes,
            timeline,
            checkout_reference,
            account_external_id,
        })
    }
}

fn build_timeline(
    payment: &PaymentRecord,
    refunds: &[RefundRecord],
    disputes: &[DisputeRecord],
) -> PaymentTimeline {
    let mut timeline = PaymentTimeline::empty();
    // The payment row always carries created_at (NOT NULL in the store);
    // captured_at is the in-memory fallback for hand-built records.
    let opened_at = || {
        payment
            .created_at
            .or(payment.captured_at)
            .unwrap_or_else(Utc::now)
    };
    timeline.push(TimelineEntry {
        kind: "created".to_string(),
        at: opened_at(),
        actor: None,
        amount: Some(payment.amount.clone()),
        detail: None,
    });
    match payment.status {
        crate::domain::PaymentStatus::Succeeded => {
            if let Some(at) = payment.captured_at {
                timeline.push(TimelineEntry {
                    kind: "succeeded".to_string(),
                    at,
                    actor: None,
                    amount: Some(payment.amount.clone()),
                    detail: None,
                });
            }
        }
        crate::domain::PaymentStatus::Failed => {
            timeline.push(TimelineEntry {
                kind: "failed".to_string(),
                at: opened_at(),
                actor: None,
                amount: Some(payment.amount.clone()),
                detail: payment.failure.as_ref().map(|f| f.code.clone()),
            });
        }
        crate::domain::PaymentStatus::Canceled => {
            timeline.push(TimelineEntry {
                kind: "canceled".to_string(),
                at: opened_at(),
                actor: None,
                amount: Some(payment.amount.clone()),
                detail: None,
            });
        }
        _ => {}
    }
    // Only money that actually moved back reads as "refunded"; a pending or
    // failed attempt stays visible in the refunds list, not the timeline.
    for refund in refunds
        .iter()
        .filter(|r| r.status == RefundStatus::Succeeded)
    {
        timeline.push(TimelineEntry {
            kind: "refunded".to_string(),
            at: refund.created_at,
            actor: refund.created_by,
            amount: Some(refund.amount.clone()),
            detail: refund.reason.clone(),
        });
    }
    for dispute in disputes {
        timeline.push(TimelineEntry {
            kind: "disputed".to_string(),
            at: dispute.created_at,
            actor: None,
            amount: Some(dispute.amount.clone()),
            detail: dispute.reason.clone(),
        });
        // A closed dispute is a second fact at a second time — flipping the
        // first entry's kind would erase when the dispute was raised.
        if !dispute.status.is_open() {
            timeline.push(TimelineEntry {
                kind: "dispute_closed".to_string(),
                at: dispute.updated_at,
                actor: None,
                amount: Some(dispute.amount.clone()),
                detail: Some(dispute.status.as_str().to_string()),
            });
        }
    }
    timeline.sort();
    timeline
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_orders_created_before_refund_before_dispute() {
        let payment = crate::domain::PaymentRecord::new(
            PaymentId::try_from(100).unwrap(),
            TenantId::try_from(101).unwrap(),
            None,
            "pi_test_1".to_string(),
            crate::domain::Money::new(2000, "JPY").unwrap(),
        )
        .unwrap();
        let mut payment = payment;
        payment.status = crate::domain::PaymentStatus::Succeeded;
        payment.captured_at = Some(Utc::now());
        let mut refund = crate::domain::payment::RefundRecord::against(
            crate::domain::payment::RefundId::try_from(102).unwrap(),
            &payment,
            500,
            Some("duplicate".to_string()),
            Some(sanvi_kernel::UserId::try_from(103).unwrap()),
            Utc::now() + chrono::Duration::seconds(10),
        )
        .unwrap();
        refund.status = RefundStatus::Succeeded;
        let dispute = crate::domain::dispute::DisputeRecord::new(
            crate::domain::dispute::DisputeId::try_from(104).unwrap(),
            payment.tenant_id,
            payment.id,
            "dp_test_1".to_string(),
            crate::domain::DisputeStatus::NeedsResponse,
            crate::domain::Money::new(2000, "JPY").unwrap(),
            Some("fraudulent".to_string()),
            None,
            Utc::now() + chrono::Duration::seconds(20),
        )
        .unwrap();
        let timeline = build_timeline(&payment, &[refund], &[dispute]);
        assert_eq!(timeline.entries[0].kind, "created");
        assert_eq!(timeline.entries[1].kind, "succeeded");
        assert_eq!(timeline.entries[2].kind, "refunded");
        assert_eq!(timeline.entries[3].kind, "disputed");
        assert_eq!(timeline.entries.len(), 4);
    }

    #[test]
    fn pending_refunds_are_not_money_moved_and_closed_disputes_close_twice() {
        let payment = crate::domain::PaymentRecord::new(
            PaymentId::try_from(100).unwrap(),
            TenantId::try_from(101).unwrap(),
            None,
            "pi_test_2".to_string(),
            crate::domain::Money::new(1000, "JPY").unwrap(),
        )
        .unwrap();
        let mut payment = payment;
        payment.status = crate::domain::PaymentStatus::Succeeded;
        payment.captured_at = Some(Utc::now());
        let mut succeeded = crate::domain::payment::RefundRecord::against(
            crate::domain::payment::RefundId::try_from(102).unwrap(),
            &payment,
            300,
            None,
            None,
            Utc::now(),
        )
        .unwrap();
        succeeded.status = RefundStatus::Succeeded;
        let pending = crate::domain::payment::RefundRecord::against(
            crate::domain::payment::RefundId::try_from(103).unwrap(),
            &payment,
            100,
            None,
            None,
            Utc::now(),
        )
        .unwrap();
        let raised_at = Utc::now() + chrono::Duration::seconds(30);
        let mut dispute = crate::domain::dispute::DisputeRecord::new(
            crate::domain::dispute::DisputeId::try_from(104).unwrap(),
            payment.tenant_id,
            payment.id,
            "dp_test_2".to_string(),
            crate::domain::DisputeStatus::Won,
            crate::domain::Money::new(1000, "JPY").unwrap(),
            None,
            None,
            raised_at,
        )
        .unwrap();
        dispute.updated_at = raised_at + chrono::Duration::seconds(30);

        let timeline = build_timeline(&payment, &[pending, succeeded], &[dispute]);
        let kinds: Vec<&str> = timeline.entries.iter().map(|e| e.kind.as_str()).collect();
        // The pending attempt never renders as money moved.
        assert_eq!(
            kinds,
            vec![
                "created",
                "succeeded",
                "refunded",
                "disputed",
                "dispute_closed"
            ]
        );
        let closed = timeline
            .entries
            .iter()
            .find(|e| e.kind == "dispute_closed")
            .unwrap();
        assert_eq!(closed.at, raised_at + chrono::Duration::seconds(30));
        assert_eq!(closed.detail.as_deref(), Some("won"));
    }
}
