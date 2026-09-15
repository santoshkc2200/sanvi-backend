//! Per-tenant rollout gating and rollback notification (TASK-017 follow-up).
//!
//! Route gating alone cannot protect background jobs: the scheduler must ask,
//! per tenant, whether `advertising.budget_guardrails` is enabled before
//! evaluating that tenant's caps — otherwise revoking the entitlement hides
//! the UI while auto-pauses keep firing. And when enforcement stops for a
//! tenant that still holds caps (global rollback or revoked entitlement),
//! the rollback path notifies that tenant: a guardrail that silently stops
//! guarding is worse than one that never existed, because the tenant has
//! stopped watching.
//!
//! "Stops" is the operative word: only a tenant whose caps were actually
//! being evaluated has something taken away. The ledger records the last
//! state observed per tenant, so the notice fires on an observed
//! serving→suspended transition — never on the first tick after a fresh
//! deploy with the flag off, when nothing was ever running to suspend.
//!
//! Suspension notices are ordinary domain events
//! (`AdEvent::BudgetGuardrailsSuspended`) published through the outbox, so
//! they travel the same Lane A path as every other budget email. Repeat
//! notices are suppressed *durably* by the
//! [`GuardrailSuspensionLedger`], not by in-process memory. The claim runs
//! in the same transaction as the publish, so a failure rolls back both and
//! the next tick retries.

use std::collections::HashSet;
use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::TxManager;

use crate::domain::model::BudgetCap;
use crate::domain::{
    AdEvent, AdvertisingError, BudgetGuardrailEntitlement, EventPublisher,
    GuardrailSuspensionLedger, TenantDirectory,
};

/// Why enforcement is suspended for a tenant. Carried on the event for the
/// audit trail; the email template does not branch on it. A *change* of
/// reason is a new transition and notifies again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspensionReason {
    /// The global `features.advertising` rollout flag is off.
    FeatureDisabled,
    /// The tenant's `advertising.budget_guardrails` entitlement is off (or
    /// its resolution failed, which fails closed).
    EntitlementOff,
}

impl SuspensionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FeatureDisabled => "feature_disabled",
            Self::EntitlementOff => "entitlement_off",
        }
    }
}

pub struct EnforcementGate {
    tx: TxManager,
    entitlements: Arc<dyn BudgetGuardrailEntitlement>,
    directory: Arc<dyn TenantDirectory>,
    events: Arc<dyn EventPublisher>,
    ledger: Arc<dyn GuardrailSuspensionLedger>,
}

impl EnforcementGate {
    pub fn new(
        tx: TxManager,
        entitlements: Arc<dyn BudgetGuardrailEntitlement>,
        directory: Arc<dyn TenantDirectory>,
        events: Arc<dyn EventPublisher>,
        ledger: Arc<dyn GuardrailSuspensionLedger>,
    ) -> Self {
        Self {
            tx,
            entitlements,
            directory,
            events,
            ledger,
        }
    }

    /// Split caps into `(evaluable, suspended_tenants)`.
    ///
    /// A tenant whose entitlement resolves to disabled — or errors, which
    /// fails closed — lands in `suspended_tenants` and none of its caps are
    /// evaluated. Callers notify the suspended set via [`Self::notify_suspended`]
    /// and evaluate only the rest. Tenants observed *serving* are recorded
    /// as serving in one audited platform-scope transaction for the whole
    /// sweep — per-tenant transactions here would cost a write transaction
    /// per entitled tenant per tick for a row that is almost always already
    /// correct — which re-arms the next suspension as a fresh transition.
    pub async fn partition(&self, caps: &[BudgetCap]) -> (Vec<BudgetCap>, Vec<TenantId>) {
        let mut tenants: Vec<TenantId> = caps.iter().map(|c| c.tenant_id).collect();
        tenants.sort_unstable();
        tenants.dedup();

        let mut suspended = HashSet::new();
        let mut serving = Vec::new();
        for tenant_id in tenants {
            match self.entitlements.is_enabled(tenant_id).await {
                Ok(true) => serving.push(tenant_id),
                Ok(false) => {
                    suspended.insert(tenant_id);
                }
                Err(e) => {
                    tracing::warn!(
                        %tenant_id,
                        error = %e,
                        "Budget guardrail entitlement check failed; failing closed (skipping evaluation)"
                    );
                    suspended.insert(tenant_id);
                }
            }
        }

        if !serving.is_empty() {
            match self
                .tx
                .begin_unscoped("advertising_budget_guardrails", "system")
                .await
            {
                Ok(mut scope) => {
                    if let Err(e) = self.ledger.mark_serving(&mut scope, &serving).await {
                        tracing::warn!(
                            error = %e,
                            tenant_count = serving.len(),
                            "Serving-marker write failed; suspension transitions may re-notify; will retry on a later tick"
                        );
                    }
                    if let Err(e) = scope.commit().await {
                        tracing::warn!(
                            error = %e,
                            "Serving-marker commit failed; will retry on a later tick"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Serving-marker transaction failed; will retry on a later tick"
                    );
                }
            }
        }

        let mut evaluable = Vec::new();
        let mut suspended_list: Vec<TenantId> = Vec::new();
        for cap in caps {
            if suspended.contains(&cap.tenant_id) {
                if !suspended_list.contains(&cap.tenant_id) {
                    suspended_list.push(cap.tenant_id);
                }
            } else {
                evaluable.push(cap.clone());
            }
        }
        (evaluable, suspended_list)
    }

    /// Publish a suspension notice per tenant, once per observed
    /// serving→suspended transition (the ledger suppresses everything else,
    /// including a first observation of an already-suspended tenant).
    /// Failures are logged and retried on a later tick; they never fail the
    /// sweep.
    pub async fn notify_suspended(&self, tenants: &[TenantId], reason: SuspensionReason) {
        for tenant_id in tenants {
            if let Err(e) = self.notify_one(*tenant_id, reason).await {
                tracing::warn!(
                    %tenant_id,
                    reason = reason.as_str(),
                    error = %e,
                    "Budget guardrail suspension notice failed; will retry on a later tick"
                );
            }
        }
    }

    /// Claim the transition in the same transaction that publishes the
    /// notice: the claim's row write commits (or rolls back) together with
    /// the outbox event, so a crash between them can neither double-send nor
    /// silently swallow a notice. An unclaimed tenant (repeat of the
    /// recorded reason) simply publishes nothing.
    async fn notify_one(
        &self,
        tenant_id: TenantId,
        reason: SuspensionReason,
    ) -> Result<(), AdvertisingError> {
        // A customer-facing email names the tenant; fall back to the id when
        // the directory read fails rather than dropping the notice.
        let tenant_name = match self.directory.display_name(tenant_id).await {
            Ok(name) => name,
            Err(e) => {
                tracing::warn!(
                    %tenant_id,
                    error = %e,
                    "Tenant directory read failed; suspension notice will name the tenant id"
                );
                tenant_id.to_string()
            }
        };

        let mut scope = self
            .tx
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        if !self
            .ledger
            .claim_notice(&mut scope, tenant_id, reason.as_str())
            .await?
        {
            // Not a transition: nothing was written, and the scope rolls
            // back harmlessly on drop.
            return Ok(());
        }
        self.events
            .publish_scoped(
                &mut scope,
                tenant_id,
                &[AdEvent::BudgetGuardrailsSuspended {
                    tenant_name,
                    reason: reason.as_str().to_string(),
                }],
            )
            .await?;
        scope
            .commit()
            .await
            .map_err(|e| AdvertisingError::Storage(e.to_string()))?;
        Ok(())
    }
}
