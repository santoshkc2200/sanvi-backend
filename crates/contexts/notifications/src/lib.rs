pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use adapters::{HttpNotificationGateway, NotificationsPrivacyProvider};
pub use application::{
    NotificationDispatcher, build_idempotency_key, email_recipient_key, map_notification_error,
    secret_fingerprint,
};
pub use config::NotificationConfig;
pub use domain::{
    CopyRenderer, EnqueueEmail, EnqueuedNotification, NotificationError, NotificationGatewayPort,
    Recipient, RecipientDirectory, RenderedNotification,
};
