//! Dispute list projection — read-only, with a Dashboard deep link.
//!
//! Disputes are never submitted through Sanvi (that would mean holding
//! evidence documents, contradicting "no PII at Sanvi"). The projection
//! stores status, amount, reason and the response deadline; the UI
//! renders a deep link to the tenant's Stripe Dashboard for handling.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use sanvi_persistence::TxManager;

use crate::domain::dispute::{DisputeRecord, DisputeStatus};
use crate::domain::error::PaymentsError;
use crate::domain::ports::{DisputeFilter, DisputeRepository, PaymentRepository};

#[derive(Debug, Clone, Default)]
pub struct ListDisputesCommand {
    pub tenant_id: Uuid,
    pub status: Option<String>,
    pub currency: Option<String>,
    pub date_from: Option<DateTime<Utc>>,
    pub date_to: Option<DateTime<Utc>>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

/// One dispute row plus its resolved Stripe Dashboard deep link — the
/// tenant's read-only handling path (Sanvi never holds evidence).
#[derive(Debug, Clone)]
pub struct DisputeListItem {
    pub record: DisputeRecord,
    pub dashboard_url: Option<String>,
}

/// One page of disputes as the API renders it.
#[derive(Debug, Clone)]
pub struct DisputePageView {
    pub items: Vec<DisputeListItem>,
    pub next_cursor: Option<String>,
}

pub struct ListDisputes {
    tx_manager: TxManager,
    disputes: Arc<dyn DisputeRepository>,
    payments: Arc<dyn PaymentRepository>,
    checkouts: Arc<dyn crate::domain::CheckoutRepository>,
    connections: Arc<dyn crate::domain::ProviderConnectionRepository>,
}

impl ListDisputes {
    pub fn new(
        tx_manager: TxManager,
        disputes: Arc<dyn DisputeRepository>,
        payments: Arc<dyn PaymentRepository>,
        checkouts: Arc<dyn crate::domain::CheckoutRepository>,
        connections: Arc<dyn crate::domain::ProviderConnectionRepository>,
    ) -> Self {
        Self {
            tx_manager,
            disputes,
            payments,
            checkouts,
            connections,
        }
    }

    pub async fn execute(
        &self,
        command: ListDisputesCommand,
    ) -> Result<DisputePageView, PaymentsError> {
        let mut filter = DisputeFilter::default();
        filter.date_from = command.date_from;
        filter.date_to = command.date_to;
        filter.cursor = command.cursor;
        filter.limit = command.limit;
        if let Some(raw) = command.status {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                // Allow any status string — unknown statuses degrade, not fail.
                filter.status = Some(DisputeStatus::parse(trimmed));
            }
        }
        if let Some(raw) = command.currency {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                filter.currency = Some(crate::domain::Currency::parse(trimmed)?);
            }
        }
        let mut scope = self
            .tx_manager
            .begin_for(command.tenant_id)
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let page = self
            .disputes
            .list(&mut scope, command.tenant_id, &filter)
            .await?;

        // Resolve each dispute's connected account (the deep-link anchor)
        // through its payment → checkout → connection chain. A dangling
        // link anywhere falls back to the tenant's live connection — only
        // a tenant with no live connection renders without links.
        let mut account_by_payment: HashMap<Uuid, Option<String>> = HashMap::new();
        for dispute in &page.items {
            if account_by_payment.contains_key(&dispute.payment_id) {
                continue;
            }
            let account = match self
                .payments
                .find_by_id(&mut scope, command.tenant_id, dispute.payment_id)
                .await?
            {
                Some(payment) => match payment.checkout_id {
                    Some(checkout_id) => match self
                        .checkouts
                        .find_by_id(&mut scope, command.tenant_id, checkout_id)
                        .await?
                    {
                        Some(checkout) => self
                            .connections
                            .find_by_id(&mut scope, command.tenant_id, checkout.connection_id)
                            .await?
                            .map(|c| c.external_account_id),
                        None => None,
                    },
                    None => None,
                },
                None => None,
            };
            account_by_payment.insert(dispute.payment_id, account);
        }
        // The fallback is resolved once per page, only when something dangles.
        let needs_fallback = page.items.iter().any(|d| {
            account_by_payment
                .get(&d.payment_id)
                .cloned()
                .flatten()
                .is_none()
        });
        let live_account = if needs_fallback {
            self.connections
                .list(&mut scope, command.tenant_id)
                .await?
                .into_iter()
                .find(|c| c.disconnected_at.is_none())
                .map(|c| c.external_account_id)
        } else {
            None
        };
        let _ = scope.rollback().await;

        Ok(DisputePageView {
            next_cursor: page.next_cursor,
            items: page
                .items
                .into_iter()
                .map(|record| {
                    let account = account_by_payment
                        .get(&record.payment_id)
                        .cloned()
                        .flatten()
                        .or_else(|| live_account.clone());
                    let dashboard_url = account.as_deref().and_then(|a| record.dashboard_url(a));
                    DisputeListItem {
                        record,
                        dashboard_url,
                    }
                })
                .collect(),
        })
    }
}
