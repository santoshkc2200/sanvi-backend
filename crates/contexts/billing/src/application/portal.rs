//! Customer Portal session creation: upgrades, downgrades, cancellation and
//! payment-method updates are Stripe-hosted; portal actions arrive back as
//! webhooks.

use std::sync::Arc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;
use serde::Serialize;
use tracing::info;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::{
    BillingError, Clock, PortalSessionRequest, StripeClient, SubscriptionRepository,
    SubscriptionSource,
};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PortalSessionView {
    pub url: String,
}

pub struct CreatePortalSession {
    tx_manager: TxManager,
    subscriptions: Arc<dyn SubscriptionRepository>,
    stripe: Arc<dyn StripeClient>,
    audit: Arc<dyn AuditWriter>,
    clock: Arc<dyn Clock>,
    return_url: String,
}

impl CreatePortalSession {
    pub fn new(
        tx_manager: TxManager,
        subscriptions: Arc<dyn SubscriptionRepository>,
        stripe: Arc<dyn StripeClient>,
        audit: Arc<dyn AuditWriter>,
        clock: Arc<dyn Clock>,
        return_url: String,
    ) -> Self {
        Self {
            tx_manager,
            subscriptions,
            stripe,
            audit,
            clock,
            return_url,
        }
    }

    pub async fn execute(
        &self,
        actor: UserId,
        tenant_id: TenantId,
    ) -> Result<PortalSessionView, BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("create_portal_session", "tenant")
            .await
            .map_err(storage)?;
        let subscription = self
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await?
            .ok_or_else(|| BillingError::SubscriptionNotFound(tenant_id.to_string()))?;
        let customer_id = match (subscription.source(), subscription.stripe_customer_id()) {
            (SubscriptionSource::Stripe, Some(customer_id)) => customer_id.to_string(),
            _ => {
                return Err(BillingError::SubscriptionNotActive);
            }
        };
        scope.commit().await.map_err(storage)?;

        let session = self
            .stripe
            .create_portal_session(&PortalSessionRequest {
                command_id: Uuid::now_v7(),
                customer_id,
                return_url: self.return_url.clone(),
            })
            .await?;

        let mut scope = self
            .tx_manager
            .begin_unscoped("audit_portal_session", "tenant")
            .await
            .map_err(storage)?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::User,
                    actor_id: Some(actor),
                    on_behalf_of: None,
                    tenant_id: Some(tenant_id),
                    action: "billing.portal_session_created".to_string(),
                    resource_type: Some("portal_session".to_string()),
                    resource_id: Some(
                        subscription
                            .stripe_subscription_id()
                            .unwrap_or_default()
                            .to_string(),
                    ),
                    before: None,
                    after: None,
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;

        info!(tenant_id = %tenant_id, "customer portal session created");
        Ok(PortalSessionView { url: session.url })
    }
}
