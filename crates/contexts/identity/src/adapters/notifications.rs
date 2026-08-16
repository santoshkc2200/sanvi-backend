//! Notification stub: phase 02 logs notifications structurally; the real
//! `notifications` context takes over delivery in a later phase. The port
//! exists so no identity code path knows (or cares) about the difference.

use async_trait::async_trait;
use tracing::info;

use crate::domain::IdentityError;
use crate::domain::ports::Notifications;
use crate::domain::values::Email;

#[derive(Debug, Clone, Copy, Default)]
pub struct LoggingNotifications;

impl LoggingNotifications {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Notifications for LoggingNotifications {
    async fn send_invitation(
        &self,
        email: &Email,
        tenant_name: &str,
        invite_url: &str,
    ) -> Result<(), IdentityError> {
        info!(
            email = %email,
            tenant = %tenant_name,
            url = %invite_url,
            notification = "invitation",
            "notification stub: invitation email"
        );
        Ok(())
    }

    async fn send_security_notice(
        &self,
        email: &Email,
        kind: &str,
        detail: &str,
    ) -> Result<(), IdentityError> {
        info!(
            email = %email,
            kind,
            detail,
            notification = "security_notice",
            "notification stub: security notice"
        );
        Ok(())
    }
}
