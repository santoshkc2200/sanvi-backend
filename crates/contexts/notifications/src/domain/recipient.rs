use async_trait::async_trait;
use sanvi_kernel::notifications::{Audience, IntentArg};

use super::error::NotificationError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipient {
    pub key: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedNotification {
    pub locale: String,
    pub subject: String,
    pub text_body: String,
}

#[async_trait]
pub trait RecipientDirectory: Send + Sync {
    async fn resolve(&self, audience: &Audience) -> Result<Vec<Recipient>, NotificationError>;
}

#[async_trait]
pub trait CopyRenderer: Send + Sync {
    async fn render(
        &self,
        audience: &Audience,
        subject_key: &str,
        body_key: &str,
        args: &[(String, IntentArg)],
    ) -> Result<RenderedNotification, NotificationError>;
}
