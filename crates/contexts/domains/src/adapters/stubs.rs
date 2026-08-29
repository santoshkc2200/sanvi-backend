//! Small stubs for composition roots that need only part of a context's
//! ports (the worker drives `configure_dns` without the purchase gate).

use async_trait::async_trait;

use sanvi_kernel::TenantId;

use crate::domain::{DomainFeatureGate, DomainOrderId, DomainsError, PurchaseCharger};

/// A gate that denies everything: for use in processes where the purchase
/// flow itself is never reachable (the worker only resumes orders; the API
/// owns the user-facing journey).
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysDisabledGate;

#[async_trait]
impl DomainFeatureGate for AlwaysDisabledGate {
    async fn is_enabled(&self, _tenant_id: TenantId, _feature: &str) -> Result<bool, DomainsError> {
        Ok(false)
    }
}

/// The reference purchase charger: deterministic sandbox charges recorded
/// in the logs (and, through the API's audit trail, in `platform.audit_log`
/// on the surrounding platform-scope transactions). A production adapter
/// implements the same port over the billing context's one-off charge
/// capability; the saga's compensation logic (refund paths, idempotency on
/// the order id) is exercised against this adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct SandboxPurchaseCharger;

#[async_trait]
impl PurchaseCharger for SandboxPurchaseCharger {
    async fn charge(
        &self,
        _tenant_id: TenantId,
        order_id: DomainOrderId,
        amount_minor: i64,
        currency: &str,
        description: &str,
    ) -> Result<String, DomainsError> {
        tracing::info!(
            order_id = %order_id,
            amount_minor,
            currency,
            description,
            "sandbox charge recorded"
        );
        Ok(format!("sandbox-ch-{order_id}"))
    }

    async fn refund(
        &self,
        _tenant_id: TenantId,
        order_id: DomainOrderId,
        charge_id: &str,
    ) -> Result<(), DomainsError> {
        tracing::info!(order_id = %order_id, charge_id, "sandbox refund recorded");
        Ok(())
    }
}
