//! Notification stub: phase 02 logs notifications structurally; the real
//! `notifications` context takes over delivery in a later phase. The port
//! exists so no identity code path knows (or cares) about the difference.

use async_trait::async_trait;
use sanvi_kernel::TenantId;
use tracing::info;

use crate::domain::IdentityError;
use crate::domain::invitation::InvitationId;
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
        tenant_id: TenantId,
        invitation_id: InvitationId,
        email: &Email,
        tenant_name: &str,
        invite_url: &str,
    ) -> Result<(), IdentityError> {
        info!(
            email = %email,
            tenant_id = %tenant_id,
            invitation_id = %invitation_id,
            tenant = %tenant_name,
            url = %invite_url,
            notification = "invitation",
            "notification stub: invitation email"
        );
        Ok(())
    }
}
