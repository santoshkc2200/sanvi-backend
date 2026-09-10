//! Maps committed privacy events to notification intents.

use sanvi_kernel::EventEnvelope;
use sanvi_kernel::notifications::{Audience, IntentArg, NotificationIntent};

pub fn notification_intent(envelope: &EventEnvelope) -> Option<NotificationIntent> {
    let email = extract_subject_email(envelope)?;
    let audience = Audience::Email(email);

    match envelope.event_type.as_str() {
        "privacy.dsr_received" => {
            let subject = envelope
                .payload
                .get("dsr_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("request");
            Some(NotificationIntent {
                audience,
                purpose: "dsr_received",
                subject_key: "emails.privacy_status.subject".to_string(),
                body_key: "emails.privacy_status.body".to_string(),
                args: vec![
                    ("subject".to_string(), IntentArg::from(subject)),
                    ("status".to_string(), IntentArg::from("received")),
                ],
            })
        }
        "privacy.dsr_closed" => {
            let subject = envelope
                .payload
                .get("dsr_kind")
                .and_then(|v| v.as_str())
                .unwrap_or("request");
            let status = envelope
                .payload
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("closed");
            Some(NotificationIntent {
                audience,
                purpose: "dsr_closed",
                subject_key: "emails.privacy_status.subject".to_string(),
                body_key: "emails.privacy_status.body".to_string(),
                args: vec![
                    ("subject".to_string(), IntentArg::from(subject)),
                    ("status".to_string(), IntentArg::from(status)),
                ],
            })
        }
        _ => None,
    }
}

fn extract_subject_email(envelope: &EventEnvelope) -> Option<String> {
    if let Some(identifiers) = envelope
        .payload
        .pointer("/subject/identifiers")
        .and_then(|v| v.as_array())
    {
        for id in identifiers {
            if id.get("type").and_then(|v| v.as_str()) == Some("email")
                && let Some(val) = id.get("value").and_then(|v| v.as_str())
            {
                return Some(val.to_string());
            }
        }
    }
    if let Some(email) = envelope
        .payload
        .pointer("/subject/key")
        .and_then(|v| v.as_str())
        .and_then(|key| key.strip_prefix("email:"))
    {
        return Some(email.to_string());
    }
    envelope
        .payload
        .get("email")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DsrId;
    use crate::domain::events::PrivacyEvent;
    use crate::domain::types::SubjectRef;
    use sanvi_kernel::{EventId, SnowflakeId};

    #[test]
    fn contract_maps_dsr_received() {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let request_id = DsrId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let subject = SubjectRef::contact("user@example.com");
        let event = PrivacyEvent::DsrReceived {
            request_id,
            subject,
            dsr_kind: "erasure".to_string(),
            jurisdiction: "gdpr".to_string(),
        };
        let envelope = event.envelope(event_id, None);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::Email("user@example.com".to_string())
        );
        assert_eq!(intent.purpose, "dsr_received");
        assert_eq!(intent.subject_key, "emails.privacy_status.subject");
        assert_eq!(intent.body_key, "emails.privacy_status.body");
        assert_eq!(
            intent.args,
            vec![
                ("subject".to_string(), IntentArg::from("erasure")),
                ("status".to_string(), IntentArg::from("received")),
            ]
        );
    }

    #[test]
    fn contract_maps_dsr_closed() {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let request_id = DsrId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let subject = SubjectRef::contact("user@example.com");
        let event = PrivacyEvent::DsrClosed {
            request_id,
            subject,
            dsr_kind: "access".to_string(),
            status: "completed".to_string(),
            jurisdiction: "gdpr".to_string(),
        };
        let envelope = event.envelope(event_id, None);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::Email("user@example.com".to_string())
        );
        assert_eq!(intent.purpose, "dsr_closed");
        assert_eq!(
            intent.args,
            vec![
                ("subject".to_string(), IntentArg::from("access")),
                ("status".to_string(), IntentArg::from("completed")),
            ]
        );
    }
}
