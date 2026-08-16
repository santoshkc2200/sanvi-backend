//! Platform subscription overrides: grant (or revoke) a plan's entitlements
//! without a Stripe subscription behind them (enterprise deals, support
//! gestures). Overrides conflict with a live Stripe subscription — money is
//! Stripe's business; access without payment is an explicit, audited
//! operator decision.

use std::sync::Arc;

use sanvi_audit::domain::{ActorType, AuditEntryData, AuditWriter};
use sanvi_persistence::TxManager;
use tracing::info;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::{
    BillingError, Clock, CollectionState, EntitlementSyncer, IdGenerator,
    PlanEntitlementRepository, PlanPriceRepository, PlanRepository, Subscription,
    SubscriptionRepository, SubscriptionSource, SubscriptionStatus,
};

const ACTOR: &str = "platform-admin";

pub struct ApplySubscriptionOverrideCommand {
    pub tenant_id: Uuid,
    pub plan_key: String,
    pub reason: String,
}

/// Live Stripe statuses block an override (portal/Stripe owns those).
fn blocks_override(status: SubscriptionStatus) -> bool {
    matches!(
        status,
        SubscriptionStatus::Trialing
            | SubscriptionStatus::Active
            | SubscriptionStatus::PastDue
            | SubscriptionStatus::Unpaid
            | SubscriptionStatus::Incomplete
            | SubscriptionStatus::Paused
    )
}

pub struct ApplySubscriptionOverride {
    tx_manager: TxManager,
    plans: Arc<dyn PlanRepository>,
    prices: Arc<dyn PlanPriceRepository>,
    grants_repo: Arc<dyn PlanEntitlementRepository>,
    subscriptions: Arc<dyn SubscriptionRepository>,
    syncer: Arc<dyn EntitlementSyncer>,
    ids: Arc<dyn IdGenerator>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

#[allow(clippy::too_many_arguments)]
impl ApplySubscriptionOverride {
    pub fn new(
        tx_manager: TxManager,
        plans: Arc<dyn PlanRepository>,
        prices: Arc<dyn PlanPriceRepository>,
        grants_repo: Arc<dyn PlanEntitlementRepository>,
        subscriptions: Arc<dyn SubscriptionRepository>,
        syncer: Arc<dyn EntitlementSyncer>,
        ids: Arc<dyn IdGenerator>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            plans,
            prices,
            grants_repo,
            subscriptions,
            syncer,
            ids,
            clock,
            audit,
        }
    }

    pub async fn execute(
        &self,
        actor: Uuid,
        cmd: ApplySubscriptionOverrideCommand,
    ) -> Result<(), BillingError> {
        if cmd.reason.trim().is_empty() {
            return Err(BillingError::Conflict(
                "an override requires a reason".to_string(),
            ));
        }
        let now = self.clock.now();
        let mut scope = self
            .tx_manager
            .begin_unscoped("apply_subscription_override", ACTOR)
            .await
            .map_err(storage)?;

        let plan = self
            .plans
            .find_by_key(&mut scope, &cmd.plan_key)
            .await?
            .ok_or_else(|| BillingError::PlanNotFound(cmd.plan_key.clone()))?;

        if let Some(existing) = self
            .subscriptions
            .find_by_tenant_platform(&mut scope, cmd.tenant_id)
            .await?
            && existing.source() == SubscriptionSource::Stripe
            && blocks_override(existing.status())
        {
            return Err(BillingError::Conflict(
                "tenant has a live Stripe subscription; use the Customer Portal or Stripe"
                    .to_string(),
            ));
        }

        // Pick the first active price for bookkeeping; entitlement grants do
        // not depend on it.
        let price_id = self
            .prices
            .list_active_for_plan(&mut scope, plan.id())
            .await?
            .first()
            .map(|price| price.id());

        let subscription = match self
            .subscriptions
            .find_by_tenant_platform(&mut scope, cmd.tenant_id)
            .await?
        {
            Some(existing) => Subscription::from_parts(
                existing.id(),
                existing.tenant_id(),
                plan.id(),
                price_id,
                SubscriptionSource::Override,
                None,
                None,
                SubscriptionStatus::Active,
                None,
                false,
                None,
                None,
                CollectionState::Ok,
                None,
                existing.created_at(),
                now,
            ),
            None => Subscription::new(
                self.ids.next_subscription_id(),
                cmd.tenant_id,
                plan.id(),
                price_id,
                SubscriptionSource::Override,
                None,
                None,
                SubscriptionStatus::Active,
                now,
            )?,
        };

        let grants = self
            .grants_repo
            .list_for_plan(&mut scope, plan.id())
            .await?;
        self.subscriptions.save(&mut scope, &subscription).await?;
        // The syncer writes the rows *and* emits EntitlementsRecomputed in
        // the same transaction (the adapter owns both).
        self.syncer
            .sync_subscription(&mut scope, cmd.tenant_id, plan.key(), &grants)
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: now,
                    actor_type: ActorType::PlatformAdmin,
                    actor_id: Some(actor),
                    on_behalf_of: None,
                    tenant_id: Some(cmd.tenant_id),
                    action: "billing.subscription_override_applied".to_string(),
                    resource_type: Some("subscription".to_string()),
                    resource_id: Some(cmd.tenant_id.to_string()),
                    before: None,
                    after: Some(serde_json::json!({
                        "plan": plan.key(),
                        "reason": cmd.reason,
                    })),
                    request_id: None,
                    ip: None,
                    user_agent: None,
                },
            )
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        scope.commit().await.map_err(storage)?;

        info!(
            tenant_id = %cmd.tenant_id,
            plan = plan.key(),
            "subscription override applied",
        );
        Ok(())
    }
}

/// Revoke an override: entitlements fall back to catalog defaults.
pub struct RevokeSubscriptionOverride {
    tx_manager: TxManager,
    subscriptions: Arc<dyn SubscriptionRepository>,
    syncer: Arc<dyn EntitlementSyncer>,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AuditWriter>,
}

impl RevokeSubscriptionOverride {
    pub fn new(
        tx_manager: TxManager,
        subscriptions: Arc<dyn SubscriptionRepository>,
        syncer: Arc<dyn EntitlementSyncer>,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AuditWriter>,
    ) -> Self {
        Self {
            tx_manager,
            subscriptions,
            syncer,
            clock,
            audit,
        }
    }

    pub async fn execute(&self, actor: Uuid, tenant_id: Uuid) -> Result<(), BillingError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("revoke_subscription_override", ACTOR)
            .await
            .map_err(storage)?;
        let Some(existing) = self
            .subscriptions
            .find_by_tenant_platform(&mut scope, tenant_id)
            .await?
        else {
            return Ok(());
        };
        if existing.source() != SubscriptionSource::Override {
            return Err(BillingError::Conflict(
                "only operator overrides can be revoked this way".to_string(),
            ));
        }
        self.syncer
            .clear_subscription(&mut scope, tenant_id)
            .await?;
        self.audit
            .append(
                &mut scope,
                &AuditEntryData {
                    occurred_at: self.clock.now(),
                    actor_type: ActorType::PlatformAdmin,
                    actor_id: Some(actor),
                    on_behalf_of: None,
                    tenant_id: Some(tenant_id),
                    action: "billing.subscription_override_revoked".to_string(),
                    resource_type: Some("subscription".to_string()),
                    resource_id: Some(tenant_id.to_string()),
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
        info!(tenant_id = %tenant_id, "subscription override revoked");
        Ok(())
    }
}
