//! The tenancy lifecycle bridge: the dunning sweep suspends tenants for
//! billing and payment recovery resumes them — but only from *our*
//! suspension. A legal or abuse suspension is never silently lifted.

use async_trait::async_trait;
use sanvi_persistence::TxManager;
use sanvi_tenancy::{
    application::{ResumeTenant, SuspendTenant},
    domain::{
        Clock as TenancyClock, EventPublisher, SuspensionReason, TenantId, TenantRepository,
        TenantStatus,
    },
};
use tracing::warn;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::{BillingError, TenantLifecycle};

pub struct TenancyTenantLifecycle {
    tx_manager: TxManager,
    tenants: std::sync::Arc<dyn TenantRepository>,
    suspend: SuspendTenant,
    resume: ResumeTenant,
}

impl TenancyTenantLifecycle {
    pub fn new(
        tx_manager: TxManager,
        tenants: std::sync::Arc<dyn TenantRepository>,
        events: std::sync::Arc<dyn EventPublisher>,
        clock: std::sync::Arc<dyn TenancyClock>,
    ) -> Self {
        let suspend = SuspendTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        );
        let resume = ResumeTenant::new(tx_manager.clone(), tenants.clone(), events, clock);
        Self {
            tx_manager,
            tenants,
            suspend,
            resume,
        }
    }
}

#[async_trait]
impl TenantLifecycle for TenancyTenantLifecycle {
    async fn suspend_billing(&self, tenant_id: Uuid) -> Result<(), BillingError> {
        let id = TenantId::from_uuid(tenant_id);
        // Already suspended (any reason) → idempotent no-op: never overwrite
        // a more serious suspension with ours.
        {
            let mut scope = self
                .tx_manager
                .begin_unscoped("billing_suspend_check", "worker")
                .await
                .map_err(storage)?;
            if let Some(tenant) = self
                .tenants
                .find_by_id(&mut scope, id)
                .await
                .map_err(|err| BillingError::Storage(err.to_string()))?
                && tenant.status() == TenantStatus::Suspended
            {
                scope.commit().await.map_err(storage)?;
                return Ok(());
            }
            scope.commit().await.map_err(storage)?;
        }
        self.suspend
            .execute(id, SuspensionReason::Billing)
            .await
            .map_err(|err| {
                warn!(tenant_id = %tenant_id, error = %err, "billing suspension failed");
                BillingError::Storage(err.to_string())
            })?;
        Ok(())
    }

    async fn resume_billing(&self, tenant_id: Uuid) -> Result<(), BillingError> {
        let id = TenantId::from_uuid(tenant_id);
        {
            let mut scope = self
                .tx_manager
                .begin_unscoped("billing_resume_check", "worker")
                .await
                .map_err(storage)?;
            let tenant = self
                .tenants
                .find_by_id(&mut scope, id)
                .await
                .map_err(|err| BillingError::Storage(err.to_string()))?;
            scope.commit().await.map_err(storage)?;
            match tenant {
                // Only our own suspension qualifies.
                Some(tenant)
                    if tenant.status() == TenantStatus::Suspended
                        && tenant.suspended_reason() == Some(SuspensionReason::Billing) => {}
                // Active or suspended for a different reason: nothing to do.
                _ => return Ok(()),
            }
        }
        self.resume
            .execute(id)
            .await
            .map_err(|err| BillingError::Storage(err.to_string()))?;
        Ok(())
    }
}
