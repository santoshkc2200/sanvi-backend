//! Dispute list projection — read-only, with a Dashboard deep link.
//!
//! Disputes are never submitted through Sanvi (that would mean holding
//! evidence documents, contradicting "no PII at Sanvi"). The projection
//! stores status, amount, reason and the response deadline; the UI
//! renders a deep link to the tenant's Stripe Dashboard for handling.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use sanvi_persistence::TxManager;

use crate::domain::dispute::DisputeStatus;
use crate::domain::error::PaymentsError;
use crate::domain::ports::{DisputeFilter, DisputeRepository};

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

pub struct ListDisputes {
    tx_manager: TxManager,
    disputes: Arc<dyn DisputeRepository>,
}

impl ListDisputes {
    pub fn new(tx_manager: TxManager, disputes: Arc<dyn DisputeRepository>) -> Self {
        Self {
            tx_manager,
            disputes,
        }
    }

    pub async fn execute(
        &self,
        command: ListDisputesCommand,
    ) -> Result<crate::domain::ports::DisputePage, PaymentsError> {
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
        let _ = scope.rollback().await;
        Ok(page)
    }
}
