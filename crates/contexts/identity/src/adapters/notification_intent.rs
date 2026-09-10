//! Maps committed identity events to notification intents.

use sanvi_kernel::EventEnvelope;
use sanvi_kernel::notifications::{Audience, IntentArg, NotificationIntent};

pub fn notification_intent(envelope: &EventEnvelope) -> Option<NotificationIntent> {
    match envelope.event_type.as_str() {
        "identity.session_revoked" => {
            let email = envelope.payload.get("email")?.as_str()?;
            Some(NotificationIntent {
                audience: Audience::Email(email.to_string()),
                purpose: "session_revoked",
                subject_key: "emails.security_notice.subject".to_string(),
                body_key: "emails.security_notice.body".to_string(),
                args: vec![(
                    "detail".to_string(),
                    IntentArg::from("a session was revoked from the account"),
                )],
            })
        }
        "identity.account_linked" | "identity.account_link_accepted" => {
            let linked_via = envelope.payload.get("linked_via").and_then(|v| v.as_str());
            if linked_via == Some("registration") {
                return None;
            }
            let email = envelope.payload.get("email")?.as_str()?;
            let provider = envelope
                .payload
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("external provider");
            let detail = format!("a new sign-in provider was linked: {provider}");

            Some(NotificationIntent {
                audience: Audience::Email(email.to_string()),
                purpose: "account_linked",
                subject_key: "emails.security_notice.subject".to_string(),
                body_key: "emails.security_notice.body".to_string(),
                args: vec![("detail".to_string(), IntentArg::from(detail))],
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::events::IdentityEvent;
    use crate::domain::user::{LinkedVia, UserId};
    use crate::domain::values::{Email, ProviderId};
    use sanvi_kernel::{EventId, SnowflakeId};

    #[test]
    fn contract_maps_session_revoked_with_email() {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let user_id = UserId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let email = Email::parse("member@example.com").unwrap();
        let event = IdentityEvent::SessionRevoked {
            user_id: Some(user_id),
            session_id: "sess_123".to_string(),
            email: Some(email),
        };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::Email("member@example.com".to_string())
        );
        assert_eq!(intent.purpose, "session_revoked");
        assert_eq!(intent.subject_key, "emails.security_notice.subject");
        assert_eq!(intent.body_key, "emails.security_notice.body");
        assert_eq!(
            intent.args,
            vec![(
                "detail".to_string(),
                IntentArg::from("a session was revoked from the account")
            )]
        );
    }

    #[test]
    fn contract_skips_session_revoked_without_email() {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = IdentityEvent::SessionRevoked {
            user_id: None,
            session_id: "sess_123".to_string(),
            email: None,
        };
        let envelope = event.envelope(event_id);
        assert!(notification_intent(&envelope).is_none());
    }

    #[test]
    fn contract_maps_account_linked_challenge() {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let user_id = UserId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let email = Email::parse("member@example.com").unwrap();
        let provider = ProviderId::parse("google").unwrap();
        let event = IdentityEvent::AccountLinked {
            user_id,
            provider,
            subject: "sub_123".to_string(),
            linked_via: LinkedVia::Challenge,
            email: Some(email),
        };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::Email("member@example.com".to_string())
        );
        assert_eq!(intent.purpose, "account_linked");
        assert_eq!(intent.subject_key, "emails.security_notice.subject");
        assert_eq!(intent.body_key, "emails.security_notice.body");
        assert_eq!(
            intent.args,
            vec![(
                "detail".to_string(),
                IntentArg::from("a new sign-in provider was linked: google")
            )]
        );
    }

    #[test]
    fn contract_maps_account_linked_auto_verified() {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let user_id = UserId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let email = Email::parse("member@example.com").unwrap();
        let provider = ProviderId::parse("github").unwrap();
        let event = IdentityEvent::AccountLinked {
            user_id,
            provider,
            subject: "sub_456".to_string(),
            linked_via: LinkedVia::AutoVerified,
            email: Some(email),
        };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "account_linked");
        assert_eq!(
            intent.args,
            vec![(
                "detail".to_string(),
                IntentArg::from("a new sign-in provider was linked: github")
            )]
        );
    }

    #[test]
    fn contract_skips_account_linked_registration() {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let user_id = UserId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let email = Email::parse("member@example.com").unwrap();
        let provider = ProviderId::parse("google").unwrap();
        let event = IdentityEvent::AccountLinked {
            user_id,
            provider,
            subject: "sub_123".to_string(),
            linked_via: LinkedVia::Registration,
            email: Some(email),
        };
        let envelope = event.envelope(event_id);
        assert!(notification_intent(&envelope).is_none());
    }
}
