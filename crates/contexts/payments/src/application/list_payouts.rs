//! The payout list (09.6): a read-only projection over the connected
//! account — schedule, next payout, recent history. Sanvi does not
//! reimplement Stripe's payout UI (the embedded component is the primary
//! surface); this endpoint exists for the tenant's payments list view and
//! for support.

use std::sync::Arc;

use serde::Serialize;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::error::PaymentsError;
use crate::domain::ports::{ProviderConnectionRepository, ProviderPayout};

/// One payout as the tenant's list renders it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PayoutView {
    pub external_payout_id: String,
    pub amount_minor: i64,
    pub currency: String,
    /// `pending` | `paid` | `failed`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arrival_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<&ProviderPayout> for PayoutView {
    fn from(payout: &ProviderPayout) -> Self {
        Self {
            external_payout_id: payout.external_payout_id.clone(),
            amount_minor: payout.amount.amount_minor,
            currency: payout.amount.currency.as_str().to_string(),
            status: payout.status.as_str().to_string(),
            arrival_at: payout.arrival_at,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PayoutsView {
    pub payouts: Vec<PayoutView>,
}

pub struct ListPayouts {
    tx_manager: TxManager,
    connections: Arc<dyn ProviderConnectionRepository>,
    registry: Arc<crate::application::ProviderRegistry>,
}

impl ListPayouts {
    pub fn new(
        tx_manager: TxManager,
        connections: Arc<dyn ProviderConnectionRepository>,
        registry: Arc<crate::application::ProviderRegistry>,
    ) -> Self {
        Self {
            tx_manager,
            connections,
            registry,
        }
    }

    /// Read the live payout history off the tenant's own connected
    /// account. No connection → an empty view, not an error: the settings
    /// page renders before onboarding completes.
    pub async fn execute(&self, tenant_id: TenantId) -> Result<PayoutsView, PaymentsError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|err| PaymentsError::Storage(err.to_string()))?;
        let connections = self.connections.list(&mut scope, tenant_id).await;
        let _ = scope.rollback().await;
        let Some(connection) = connections?
            .into_iter()
            .find(|c| c.disconnected_at.is_none())
        else {
            return Ok(PayoutsView::default());
        };
        let adapter = self.registry.get(connection.provider).ok_or_else(|| {
            PaymentsError::InvalidConnection(format!(
                "provider {:?} is not available in this environment",
                connection.provider.as_str()
            ))
        })?;

        // Newest first is the provider's own ordering for this listing.
        let payouts = adapter
            .list_payouts(&connection.external_account_id, None)
            .await?;
        Ok(PayoutsView {
            payouts: payouts.iter().map(PayoutView::from).collect(),
        })
    }
}
