//! Maps committed billing events to notification intents.

use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use sanvi_kernel::TenantId;
use sanvi_kernel::notifications::{Audience, IntentArg, NotificationIntent};

const BILLING_ROLES: &[&str] = &["tenant_owner", "tenant_admin", "tenant_billing"];

pub fn notification_intent(envelope: &EventEnvelope) -> Option<NotificationIntent> {
    let tenant_id = extract_tenant_id(envelope)?;
    let audience = Audience::TenantRoles {
        tenant_id,
        roles: BILLING_ROLES,
    };

    match envelope.event_type.as_str() {
        "billing.invoice_payment_failed" => Some(NotificationIntent {
            audience,
            purpose: "payment_failed",
            subject_key: "emails.payment_failed.subject".to_string(),
            body_key: "emails.payment_failed.body".to_string(),
            args: vec![],
        }),
        "billing.subscription_suspended" => Some(NotificationIntent {
            audience,
            purpose: "grace_expired",
            subject_key: "emails.grace_expired.subject".to_string(),
            body_key: "emails.grace_expired.body".to_string(),
            args: vec![],
        }),
        "billing.trial_ending" => {
            let trial_end_str = envelope.payload.get("trial_end")?.as_str()?;
            let trial_end = trial_end_str.parse::<DateTime<Utc>>().ok()?;

            Some(NotificationIntent {
                audience,
                purpose: "trial_ending",
                subject_key: "emails.trial_ending.subject".to_string(),
                body_key: "emails.trial_ending.body".to_string(),
                args: vec![("date".to_string(), IntentArg::Timestamp(trial_end))],
            })
        }
        "billing.subscription_canceled" => Some(NotificationIntent {
            audience,
            purpose: "subscription_canceled",
            subject_key: "emails.subscription_canceled.subject".to_string(),
            body_key: "emails.subscription_canceled.body".to_string(),
            args: vec![],
        }),
        "billing.subscription_restored" => Some(NotificationIntent {
            audience,
            purpose: "subscription_restored",
            subject_key: "emails.subscription_restored.subject".to_string(),
            body_key: "emails.subscription_restored.body".to_string(),
            args: vec![],
        }),
        _ => None,
    }
}

fn extract_tenant_id(envelope: &EventEnvelope) -> Option<TenantId> {
    envelope
        .metadata
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<TenantId>().ok())
        .or_else(|| {
            envelope
                .payload
                .get("tenant_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<TenantId>().ok())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::BillingEvent;
    use chrono::Utc;
    use sanvi_kernel::{EventId, SnowflakeId};

    #[test]
    fn contract_maps_invoice_payment_failed() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = BillingEvent::InvoicePaymentFailed {
            tenant_id,
            number: Some("INV-001".to_string()),
            total_minor: 5000,
        };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::TenantRoles {
                tenant_id,
                roles: BILLING_ROLES,
            }
        );
        assert_eq!(intent.purpose, "payment_failed");
        assert_eq!(intent.subject_key, "emails.payment_failed.subject");
        assert_eq!(intent.body_key, "emails.payment_failed.body");
    }

    #[test]
    fn contract_maps_subscription_suspended() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = BillingEvent::SubscriptionSuspended {
            tenant_id,
            stripe_subscription_id: "sub_123".to_string(),
        };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "grace_expired");
        assert_eq!(intent.subject_key, "emails.grace_expired.subject");
    }

    #[test]
    fn contract_maps_trial_ending() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let now = Utc::now();
        let event = BillingEvent::TrialEnding {
            tenant_id,
            trial_end: now,
        };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "trial_ending");
        assert_eq!(intent.args.len(), 1);
        assert_eq!(intent.args[0].0, "date");
    }

    #[test]
    fn contract_maps_subscription_canceled() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = BillingEvent::SubscriptionCanceled { tenant_id };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "subscription_canceled");
    }

    #[test]
    fn contract_maps_subscription_restored() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = BillingEvent::SubscriptionRestored { tenant_id };
        let envelope = event.envelope(event_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "subscription_restored");
    }
}
