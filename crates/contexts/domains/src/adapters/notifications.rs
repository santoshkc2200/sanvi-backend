//! Tenant-facing notifications: a logging stub today; the notifications
//! context takes over delivery in a later phase.

use async_trait::async_trait;

use crate::domain::{DomainsError, Notifications};

#[derive(Debug, Clone, Copy, Default)]
pub struct LogNotifications;

impl LogNotifications {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Notifications for LogNotifications {
    async fn domain_degraded(
        &self,
        hostname: &str,
        code: &str,
        detail: &str,
    ) -> Result<(), DomainsError> {
        tracing::warn!(
            hostname,
            code,
            detail,
            "tenant notification: domain degraded"
        );
        Ok(())
    }

    async fn certificate_renewal_failed(
        &self,
        hostname: &str,
        detail: &str,
    ) -> Result<(), DomainsError> {
        tracing::error!(
            hostname,
            detail,
            "tenant notification: certificate renewal failed"
        );
        Ok(())
    }

    async fn domain_expiring_soon(
        &self,
        hostname: &str,
        days_left: u32,
    ) -> Result<(), DomainsError> {
        tracing::warn!(
            hostname,
            days_left,
            "tenant notification: domain expiring soon"
        );
        Ok(())
    }

    async fn order_failed(&self, hostname: &str, detail: &str) -> Result<(), DomainsError> {
        tracing::warn!(hostname, detail, "tenant notification: domain order failed");
        Ok(())
    }

    async fn domain_removed(&self, hostname: &str, reason: &str) -> Result<(), DomainsError> {
        tracing::info!(hostname, reason, "tenant notification: domain removed");
        Ok(())
    }
}
