//! Notification stub: phase 04 logs notifications structurally; the real
//! `notifications` context takes over delivery in a later phase. The port
//! exists so no billing code path knows (or cares) about the difference.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::TenantId;
use tracing::info;

use crate::domain::{BillingError, Notifications};

#[derive(Debug, Clone, Copy, Default)]
pub struct LoggingNotifications;

impl LoggingNotifications {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Notifications for LoggingNotifications {
    async fn payment_failed(&self, tenant_id: TenantId) -> Result<(), BillingError> {
        info!(
            tenant_id = %tenant_id,
            notification = "billing.payment_failed",
            "notification stub: payment failed"
        );
        Ok(())
    }

    async fn grace_expired(&self, tenant_id: TenantId) -> Result<(), BillingError> {
        info!(
            tenant_id = %tenant_id,
            notification = "billing.grace_expired",
            "notification stub: dunning grace expired"
        );
        Ok(())
    }

    async fn trial_ending(
        &self,
        tenant_id: TenantId,
        trial_end: DateTime<Utc>,
    ) -> Result<(), BillingError> {
        info!(
            tenant_id = %tenant_id,
            trial_end = %trial_end,
            notification = "billing.trial_ending",
            "notification stub: trial ending"
        );
        Ok(())
    }

    async fn subscription_canceled(&self, tenant_id: TenantId) -> Result<(), BillingError> {
        info!(
            tenant_id = %tenant_id,
            notification = "billing.subscription_canceled",
            "notification stub: subscription canceled"
        );
        Ok(())
    }

    async fn subscription_restored(&self, tenant_id: TenantId) -> Result<(), BillingError> {
        info!(
            tenant_id = %tenant_id,
            notification = "billing.subscription_restored",
            "notification stub: subscription restored"
        );
        Ok(())
    }
}
