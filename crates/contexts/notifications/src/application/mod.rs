pub mod dispatcher;

pub use dispatcher::{
    NotificationDispatcher, build_idempotency_key, email_recipient_key, map_notification_error,
    secret_fingerprint,
};
