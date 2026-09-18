use chrono::Utc;
use sanvi_kernel::EventEnvelope;
use sanvi_kernel::notifications::{Audience, NotificationIntent};
use sanvi_messaging::{ConsumerHandler, HandlerError};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::domain::{
    CopyRenderer, EnqueueEmail, NotificationError, NotificationGatewayPort, RecipientDirectory,
};

/// Builds a deterministic idempotency key for the notification service:
/// `{event_type}:{event_id}:{purpose}:{recipient_key}`
/// Charset: `^[!-~]+$`, Length: <= 128 chars.
pub fn build_idempotency_key(
    event_type: &str,
    event_id: &str,
    purpose: &str,
    recipient_key: &str,
) -> String {
    format!("{event_type}:{event_id}:{purpose}:{recipient_key}")
}

/// A 16-hex-character fingerprint of a value, for use as an idempotency-key
/// segment.
///
/// Lane B (credential-bearing) sends fingerprint the credential itself: a
/// transport retry of the same token or code produces the same key and is
/// replayed by the service, while a reissue — a resent invitation, a fresh
/// verification code — produces a different key and is correctly a new
/// message. The fingerprint is one-way, so the key never carries the secret.
pub fn secret_fingerprint(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let hash = hex::encode(hasher.finalize());
    hash[..16].to_string()
}

/// Hashes an email address to a 16-character hex prefix for stable idempotency keying.
pub fn email_recipient_key(email: &str) -> String {
    secret_fingerprint(email)
}

pub struct NotificationDispatcher {
    gateway: Arc<dyn NotificationGatewayPort>,
    directory: Arc<dyn RecipientDirectory>,
    renderer: Arc<dyn CopyRenderer>,
    intent_mapper: fn(&EventEnvelope) -> Option<NotificationIntent>,
    max_event_age_secs: u64,
}

impl NotificationDispatcher {
    pub fn new(
        gateway: Arc<dyn NotificationGatewayPort>,
        directory: Arc<dyn RecipientDirectory>,
        renderer: Arc<dyn CopyRenderer>,
        intent_mapper: fn(&EventEnvelope) -> Option<NotificationIntent>,
        max_event_age_secs: u64,
    ) -> Self {
        Self {
            gateway,
            directory,
            renderer,
            intent_mapper,
            max_event_age_secs,
        }
    }

    #[tracing::instrument(name = "notifications.handle_envelope", skip_all)]
    pub async fn handle_envelope(&self, envelope: EventEnvelope) -> anyhow::Result<()> {
        let age_secs = (Utc::now() - envelope.occurred_at).num_seconds();
        if age_secs > self.max_event_age_secs as i64 {
            tracing::warn!(
                event_id = %envelope.id,
                event_type = %envelope.event_type,
                age_secs,
                max_age = self.max_event_age_secs,
                "dropping notification event older than max_event_age_secs"
            );
            return Ok(());
        }

        let intent = match (self.intent_mapper)(&envelope) {
            Some(intent) => intent,
            None => return Ok(()),
        };

        let recipients = match self.directory.resolve(&intent.audience).await {
            Ok(recipients) => recipients,
            Err(err) => return Err(map_notification_error(err)),
        };

        if recipients.is_empty() {
            tracing::warn!(
                event_id = %envelope.id,
                event_type = %envelope.event_type,
                purpose = %intent.purpose,
                "notification audience resolved to zero recipients"
            );
            return Ok(());
        }

        let rendered = match self
            .renderer
            .render(
                &intent.audience,
                &intent.subject_key,
                &intent.body_key,
                &intent.args,
            )
            .await
        {
            Ok(rendered) => rendered,
            Err(err) => return Err(map_notification_error(err)),
        };

        let tenant_id_owned = match &intent.audience {
            Audience::TenantRoles { tenant_id, .. } => Some(tenant_id.to_string()),
            Audience::Platform => None,
            Audience::Email(_) => envelope
                .metadata
                .get("tenant_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        for recipient in &recipients {
            let recipient_key = if recipient.key.is_empty() {
                email_recipient_key(&recipient.email)
            } else {
                recipient.key.clone()
            };

            let idempotency_key = build_idempotency_key(
                &envelope.event_type,
                &envelope.id.to_string(),
                intent.purpose,
                &recipient_key,
            );

            let enqueue_req = EnqueueEmail {
                tenant_id: tenant_id_owned.as_deref(),
                recipient: &recipient.email,
                subject: &rendered.subject,
                text_body: &rendered.text_body,
                html_body: None,
                idempotency_key: &idempotency_key,
            };

            if let Err(err) = self.gateway.enqueue_email(enqueue_req).await {
                return Err(map_notification_error(err));
            }
        }

        Ok(())
    }

    pub fn into_consumer_handler(self: Arc<Self>) -> ConsumerHandler {
        Arc::new(move |envelope: EventEnvelope| {
            let dispatcher = self.clone();
            Box::pin(async move { dispatcher.handle_envelope(envelope).await })
        })
    }
}

pub fn map_notification_error(err: NotificationError) -> anyhow::Error {
    match err {
        NotificationError::Unavailable { detail } => {
            HandlerError::transient(anyhow::anyhow!(detail)).into()
        }
        NotificationError::Conflict { detail } => {
            tracing::error!(%detail, "permanent notification conflict: idempotency key collision");
            HandlerError::permanent(anyhow::anyhow!(detail)).into()
        }
        NotificationError::Invalid { code, detail } => {
            tracing::warn!(%code, %detail, "permanent notification validation error");
            HandlerError::permanent(anyhow::anyhow!("{code}: {detail}")).into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use chrono::Duration;
    use sanvi_kernel::EventEnvelope;
    use sanvi_kernel::notifications::{Audience, IntentArg, NotificationIntent};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_idempotency_key_format_and_charset() {
        let key = build_idempotency_key(
            "payments.provider_connection_restricted",
            "7231456789012345",
            "restricted",
            "7244987654321098",
        );
        assert!(key.len() <= 128);
        assert!(key.chars().all(|c| c.is_ascii_graphic()));
        assert_eq!(
            key,
            "payments.provider_connection_restricted:7231456789012345:restricted:7244987654321098"
        );
    }

    #[test]
    fn test_email_recipient_key_stability() {
        let key1 = email_recipient_key("user@example.com");
        let key2 = email_recipient_key("user@example.com");
        assert_eq!(key1, key2);
        assert_eq!(key1.len(), 16);
    }

    struct MockGateway {
        calls: AtomicUsize,
        result: Result<(), NotificationError>,
    }

    #[async_trait]
    impl NotificationGatewayPort for MockGateway {
        async fn enqueue_email(
            &self,
            _email: EnqueueEmail<'_>,
        ) -> Result<crate::domain::EnqueuedNotification, NotificationError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result
                .clone()
                .map(|_| crate::domain::EnqueuedNotification {
                    id: "notif-1".to_string(),
                    status: "queued".to_string(),
                    idempotent_replay: false,
                })
        }
    }

    struct MockDirectory;
    #[async_trait]
    impl RecipientDirectory for MockDirectory {
        async fn resolve(
            &self,
            _audience: &Audience,
        ) -> Result<Vec<crate::domain::Recipient>, NotificationError> {
            Ok(vec![crate::domain::Recipient {
                key: "user-1".to_string(),
                email: "user@example.com".to_string(),
            }])
        }
    }

    struct MockRenderer;
    #[async_trait]
    impl CopyRenderer for MockRenderer {
        async fn render(
            &self,
            _audience: &Audience,
            _subject_key: &str,
            _body_key: &str,
            _args: &[(String, IntentArg)],
        ) -> Result<crate::domain::RenderedNotification, NotificationError> {
            Ok(crate::domain::RenderedNotification {
                locale: "en".to_string(),
                subject: "Test Subject".to_string(),
                text_body: "Test Body".to_string(),
            })
        }
    }

    fn sample_mapper(_envelope: &EventEnvelope) -> Option<NotificationIntent> {
        Some(NotificationIntent {
            audience: Audience::Email("user@example.com".to_string()),
            purpose: "test_purpose",
            subject_key: "emails.test.subject".to_string(),
            body_key: "emails.test.body".to_string(),
            args: vec![],
        })
    }

    #[tokio::test]
    async fn drops_events_older_than_max_age() {
        let gateway = Arc::new(MockGateway {
            calls: AtomicUsize::new(0),
            result: Ok(()),
        });
        let dispatcher = NotificationDispatcher::new(
            gateway.clone(),
            Arc::new(MockDirectory),
            Arc::new(MockRenderer),
            sample_mapper,
            3600,
        );

        let old_envelope = EventEnvelope {
            id: sanvi_kernel::EventId::try_from(1i64).unwrap(),
            aggregate: "test".to_string(),
            aggregate_id: sanvi_kernel::SnowflakeId::try_from(1i64).unwrap(),
            event_type: "test.event".to_string(),
            occurred_at: Utc::now() - Duration::seconds(7200),
            payload: serde_json::json!({}),
            metadata: serde_json::json!({}),
        };

        let res = dispatcher.handle_envelope(old_envelope).await;
        assert!(res.is_ok());
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn handles_recent_event_successfully() {
        let gateway = Arc::new(MockGateway {
            calls: AtomicUsize::new(0),
            result: Ok(()),
        });
        let dispatcher = NotificationDispatcher::new(
            gateway.clone(),
            Arc::new(MockDirectory),
            Arc::new(MockRenderer),
            sample_mapper,
            3600,
        );

        let recent_envelope = EventEnvelope {
            id: sanvi_kernel::EventId::try_from(2i64).unwrap(),
            aggregate: "test".to_string(),
            aggregate_id: sanvi_kernel::SnowflakeId::try_from(2i64).unwrap(),
            event_type: "test.event".to_string(),
            occurred_at: Utc::now() - Duration::seconds(60),
            payload: serde_json::json!({}),
            metadata: serde_json::json!({}),
        };

        let res = dispatcher.handle_envelope(recent_envelope).await;
        assert!(res.is_ok());
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_error_mapping_to_handler_error() {
        let transient_err = NotificationError::Unavailable {
            detail: "timeout".to_string(),
        };
        let err = map_notification_error(transient_err);
        assert!(sanvi_messaging::is_transient_error(&err));

        let conflict_err = NotificationError::Conflict {
            detail: "payload changed".to_string(),
        };
        let err = map_notification_error(conflict_err);
        assert!(!sanvi_messaging::is_transient_error(&err));

        let invalid_err = NotificationError::Invalid {
            code: "bad_request".to_string(),
            detail: "invalid recipient".to_string(),
        };
        let err = map_notification_error(invalid_err);
        assert!(!sanvi_messaging::is_transient_error(&err));
    }
}
