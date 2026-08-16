//! Daily reconciliation: Stripe is authoritative for money, so the local
//! projection must be re-derived from it — webhook outages silently freeze
//! entitlements otherwise. This job re-reads live subscriptions, applies
//! diffs through the same update path webhooks use, and counts drift.

use std::sync::Arc;

use sanvi_persistence::TxManager;
use serde::Serialize;
use tracing::{info, warn};
use utoipa::ToSchema;

use crate::application::storage;
use crate::application::webhooks::WebhookDispatcher;
use crate::domain::{BillingError, StripeClient, StripeSubscriptionData, SubscriptionRepository};

#[derive(Debug, Clone, Copy, Default, Serialize, ToSchema)]
pub struct ReconciliationReport {
    /// Subscriptions re-read from Stripe.
    pub checked: u32,
    /// Local rows whose state diverged from Stripe and was repaired.
    pub drift_repaired: u32,
    /// Stripe reads that failed (retried on the next pass).
    pub errors: u32,
}

/// Re-derives local subscription state from Stripe. It reuses the webhook
/// dispatcher's update path — the same code that handles live events repairs
/// drift, so the two can never disagree about semantics.
pub struct ReconcileSubscriptions {
    tx_manager: TxManager,
    subscriptions: Arc<dyn SubscriptionRepository>,
    stripe: Arc<dyn StripeClient>,
    dispatcher: Arc<WebhookDispatcher>,
}

impl ReconcileSubscriptions {
    pub fn new(
        tx_manager: TxManager,
        subscriptions: Arc<dyn SubscriptionRepository>,
        stripe: Arc<dyn StripeClient>,
        dispatcher: Arc<WebhookDispatcher>,
    ) -> Self {
        Self {
            tx_manager,
            subscriptions,
            stripe,
            dispatcher,
        }
    }

    pub async fn execute(&self, limit: u32) -> Result<ReconciliationReport, BillingError> {
        let candidates = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("reconciliation_fetch", "worker")
                .await
                .map_err(storage)?;
            let candidates = self
                .subscriptions
                .list_reconcilable(&mut scope, limit)
                .await?;
            scope.commit().await.map_err(storage)?;
            candidates
        };

        let mut report = ReconciliationReport::default();
        for subscription in candidates {
            let Some(stripe_subscription_id) = subscription.stripe_subscription_id() else {
                continue;
            };
            report.checked += 1;
            let view = match self
                .stripe
                .retrieve_subscription(stripe_subscription_id)
                .await
            {
                Ok(view) => view,
                Err(err) => {
                    report.errors += 1;
                    warn!(
                        tenant_id = %subscription.tenant_id(),
                        stripe_subscription_id,
                        error = %err,
                        "reconciliation could not read subscription from Stripe",
                    );
                    continue;
                }
            };

            let drifted = drifted(&subscription, &view);
            let data = StripeSubscriptionData {
                id: view.id.clone(),
                customer_id: view.customer_id.clone(),
                status: view.status,
                stripe_price_id: view.stripe_price_id.clone(),
                current_period: view.current_period,
                cancel_at_period_end: view.cancel_at_period_end,
                trial_end: view.trial_end,
                metadata: serde_json::json!({}),
                created: view.updated.unwrap_or_else(chrono::Utc::now),
            };
            let mut scope = self
                .tx_manager
                .begin_unscoped("reconciliation_apply", "worker")
                .await
                .map_err(storage)?;
            let applied = self
                .dispatcher
                .apply_subscription_update(&mut scope, &data, data.created)
                .await?;
            scope.commit().await.map_err(storage)?;

            if applied == crate::application::webhooks::ApplyOutcome::Applied || drifted {
                report.drift_repaired += 1;
                info!(
                    tenant_id = %subscription.tenant_id(),
                    stripe_subscription_id,
                    "reconciliation repaired subscription drift",
                );
            }
        }
        Ok(report)
    }
}

/// Local vs Stripe comparison across the entitlement-bearing fields
/// (status and period; the price id is the plan's fingerprint).
fn drifted(
    local: &crate::domain::Subscription,
    remote: &crate::domain::StripeSubscriptionView,
) -> bool {
    local.status() != remote.status
        || local.current_period() != remote.current_period
        || local.cancel_at_period_end() != remote.cancel_at_period_end
        || local.trial_end() != remote.trial_end
}
