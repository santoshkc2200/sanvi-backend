use async_trait::async_trait;

use super::error::NotificationError;

/// Parameters for enqueuing one rendered email.
#[derive(Debug, Clone)]
pub struct EnqueueEmail<'a> {
    pub tenant_id: Option<&'a str>,
    pub recipient: &'a str,
    pub subject: &'a str,
    pub text_body: &'a str,
    pub html_body: Option<&'a str>,
    pub idempotency_key: &'a str,
}

/// Metadata returned upon successful enqueue (or idempotent replay).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueuedNotification {
    pub id: String,
    pub status: String,
    pub idempotent_replay: bool,
}

/// Outbound port to the external notification service.
#[async_trait]
pub trait NotificationGatewayPort: Send + Sync {
    async fn enqueue_email(
        &self,
        email: EnqueueEmail<'_>,
    ) -> Result<EnqueuedNotification, NotificationError>;
}
