//! The dunning sweep: grace windows expire here, not in request paths.
//!
//! `invoice.payment_failed` starts the clock (collection_state=dunning);
//! this scheduled job finds dunning rows whose grace window has elapsed,
//! downgrades their entitlements to the limited set, suspends the tenant
//! with the billing reason and notifies. Recovery (`invoice.paid`) restores
//! everything. The trial reminder runs in the same sweep.

use std::sync::Arc;

use chrono::Duration;
use sanvi_persistence::TxManager;
use serde::Serialize;
use tracing::{info, warn};
use utoipa::ToSchema;

use crate::application::storage;
use crate::domain::{
    BillingError, Clock, CollectionState, EntitlementSyncer, EventPublisher, Notifications,
    SubscriptionRepository, SubscriptionStatus, TenantLifecycle,
};

#[derive(Debug, Clone, Copy, Default, Serialize, ToSchema)]
pub struct DunningSweepReport {
    /// Tenants suspended after grace expiry this pass, including retries
    /// after a prior cross-context suspension failure.
    pub grace_expired: u32,
    /// Trial-ending notices sent this pass.
    pub trials_notified: u32,
}

pub struct DunningSweep {
    tx_manager: TxManager,
    subscriptions: Arc<dyn SubscriptionRepository>,
    syncer: Arc<dyn EntitlementSyncer>,
    lifecycle: Arc<dyn TenantLifecycle>,
    notifications: Arc<dyn Notifications>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    grace_days: u32,
    trial_reminder_days: i64,
}

impl DunningSweep {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_manager: TxManager,
        subscriptions: Arc<dyn SubscriptionRepository>,
        syncer: Arc<dyn EntitlementSyncer>,
        lifecycle: Arc<dyn TenantLifecycle>,
        notifications: Arc<dyn Notifications>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        grace_days: u32,
        trial_reminder_days: i64,
    ) -> Self {
        Self {
            tx_manager,
            subscriptions,
            syncer,
            lifecycle,
            notifications,
            events,
            clock,
            grace_days,
            trial_reminder_days,
        }
    }

    pub async fn execute(&self, limit: u32) -> Result<DunningSweepReport, BillingError> {
        Ok(DunningSweepReport {
            grace_expired: self.expire_grace(limit).await?,
            trials_notified: self.notify_trials(limit).await?,
        })
    }

    async fn expire_grace(&self, limit: u32) -> Result<u32, BillingError> {
        let now = self.clock.now();
        let deadline = now - Duration::days(i64::from(self.grace_days));
        let due = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("dunning_sweep_fetch", "worker")
                .await
                .map_err(storage)?;
            let due = self
                .subscriptions
                .list_dunning_due(&mut scope, deadline, limit)
                .await?;
            scope.commit().await.map_err(storage)?;
            due
        };

        let mut expired = 0;
        for candidate in due {
            let tenant_id = candidate.tenant_id();
            {
                let mut scope = self
                    .tx_manager
                    .begin_unscoped("dunning_sweep_expire", "worker")
                    .await
                    .map_err(storage)?;
                // Re-read inside the write transaction so a payment recovery
                // that landed after the fetch cannot be overwritten by the
                // stale candidate snapshot.
                let Some(mut subscription) = self
                    .subscriptions
                    .find_by_tenant_platform(&mut scope, tenant_id)
                    .await?
                else {
                    scope.commit().await.map_err(storage)?;
                    continue;
                };
                let newly_expired = subscription.expire_grace(now);
                if !newly_expired
                    && subscription.collection_state() != CollectionState::GraceExpired
                {
                    // Another sweep repaired or otherwise changed this row.
                    scope.commit().await.map_err(storage)?;
                    continue;
                }
                if newly_expired {
                    self.subscriptions.save(&mut scope, &subscription).await?;
                    self.syncer
                        .clear_subscription(&mut scope, tenant_id)
                        .await?;
                    self.events
                        .publish(
                            &mut scope,
                            &[crate::domain::BillingEvent::SubscriptionSuspended {
                                tenant_id,
                                stripe_subscription_id: subscription
                                    .stripe_subscription_id()
                                    .unwrap_or_default()
                                    .to_string(),
                            }],
                        )
                        .await?;
                }
                scope.commit().await.map_err(storage)?;
            }
            // Cross-context: suspend the tenant (own transaction). A failure
            // here leaves entitlements downgraded but the tenant active —
            // fail-closed for revenue, repaired by the next sweep.
            if let Err(err) = self.lifecycle.suspend_billing(tenant_id).await {
                warn!(tenant_id = %tenant_id, error = %err, "grace expired but tenant suspension failed; next sweep retries");
                continue;
            }
            self.notifications.grace_expired(tenant_id).await?;
            expired += 1;
            info!(tenant_id = %tenant_id, "dunning grace expired; tenant suspended");
        }
        Ok(expired)
    }

    async fn notify_trials(&self, limit: u32) -> Result<u32, BillingError> {
        let now = self.clock.now();
        let within = Duration::days(self.trial_reminder_days);
        let due = {
            let mut scope = self
                .tx_manager
                .begin_unscoped("trial_sweep_fetch", "worker")
                .await
                .map_err(storage)?;
            let due = self
                .subscriptions
                .list_trials_ending_soon(&mut scope, now, within, limit)
                .await?;
            scope.commit().await.map_err(storage)?;
            due
        };

        let mut notified = 0;
        for subscription in due {
            let tenant_id = subscription.tenant_id();
            let Some(trial_end) = subscription.trial_end() else {
                continue;
            };
            {
                let mut scope = self
                    .tx_manager
                    .begin_unscoped("trial_sweep_notify", "worker")
                    .await
                    .map_err(storage)?;
                let mut subscription = subscription;
                subscription.mark_trial_notified(now);
                self.subscriptions.save(&mut scope, &subscription).await?;
                self.events
                    .publish(
                        &mut scope,
                        &[crate::domain::BillingEvent::TrialEnding {
                            tenant_id,
                            trial_end,
                        }],
                    )
                    .await?;
                scope.commit().await.map_err(storage)?;
            }
            self.notifications
                .trial_ending(tenant_id, trial_end)
                .await?;
            notified += 1;
        }
        Ok(notified)
    }

    /// The statuses the trial sweep considers (helper for the repository
    /// query documentation; kept here so the sweep owns its own rules).
    #[allow(dead_code)]
    fn trial_statuses() -> [SubscriptionStatus; 1] {
        [SubscriptionStatus::Trialing]
    }
}
