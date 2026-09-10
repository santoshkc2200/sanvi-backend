//! Maps committed payments events to notification intents.

use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use sanvi_kernel::TenantId;
use sanvi_kernel::notifications::{Audience, IntentArg, NotificationIntent};

const PAYMENTS_ROLES: &[&str] = &["tenant_owner", "tenant_admin", "tenant_billing"];

pub fn notification_intent(envelope: &EventEnvelope) -> Option<NotificationIntent> {
    let tenant_id = extract_tenant_id(envelope)?;
    let audience = Audience::TenantRoles {
        tenant_id,
        roles: PAYMENTS_ROLES,
    };

    match envelope.event_type.as_str() {
        "payments.provider_connection_activated" => {
            let connection_id = envelope.payload.get("connection_id")?.as_str()?;
            Some(NotificationIntent {
                audience,
                purpose: "connection_activated",
                subject_key: "emails.payments_connection_activated.subject".to_string(),
                body_key: "emails.payments_connection_activated.body".to_string(),
                args: vec![("connection".to_string(), IntentArg::from(connection_id))],
            })
        }
        "payments.provider_connection_restricted" => {
            let connection_id = envelope.payload.get("connection_id")?.as_str()?;
            let detail = envelope
                .payload
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            Some(NotificationIntent {
                audience,
                purpose: "connection_restricted",
                subject_key: "emails.payments_connection_restricted.subject".to_string(),
                body_key: "emails.payments_connection_restricted.body".to_string(),
                args: vec![
                    ("connection".to_string(), IntentArg::from(connection_id)),
                    ("detail".to_string(), IntentArg::from(detail)),
                ],
            })
        }
        "payments.requirement_deadline_approaching" => {
            let connection_id = envelope.payload.get("connection_id")?.as_str()?;
            let deadline_str = envelope.payload.get("deadline")?.as_str()?;
            let deadline = deadline_str.parse::<DateTime<Utc>>().ok()?;
            Some(NotificationIntent {
                audience,
                purpose: "requirement_deadline",
                subject_key: "emails.payments_requirement_deadline.subject".to_string(),
                body_key: "emails.payments_requirement_deadline.body".to_string(),
                args: vec![
                    ("connection".to_string(), IntentArg::from(connection_id)),
                    ("date".to_string(), IntentArg::from(deadline)),
                ],
            })
        }
        "payments.dispute_opened" => {
            let dispute_id = envelope.payload.get("dispute_id")?.as_str()?;
            let due_by = envelope
                .payload
                .get("due_by")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<DateTime<Utc>>().ok());

            match due_by {
                Some(due) => Some(NotificationIntent {
                    audience,
                    purpose: "dispute_opened",
                    subject_key: "emails.payments_dispute_opened.subject".to_string(),
                    body_key: "emails.payments_dispute_opened.body_due".to_string(),
                    args: vec![
                        ("dispute".to_string(), IntentArg::from(dispute_id)),
                        ("due".to_string(), IntentArg::from(due)),
                    ],
                }),
                None => Some(NotificationIntent {
                    audience,
                    purpose: "dispute_opened",
                    subject_key: "emails.payments_dispute_opened.subject".to_string(),
                    body_key: "emails.payments_dispute_opened.body".to_string(),
                    args: vec![("dispute".to_string(), IntentArg::from(dispute_id))],
                }),
            }
        }
        "payments.payout_failed" => {
            let payout = envelope
                .payload
                .get("external_payout_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let amount_minor = envelope
                .payload
                .pointer("/amount/amount_minor")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let currency = envelope
                .payload
                .pointer("/amount/currency")
                .and_then(|v| v.as_str())
                .unwrap_or("USD");
            let amount_str = format!("{amount_minor} {currency}");

            Some(NotificationIntent {
                audience,
                purpose: "payout_failed",
                subject_key: "emails.payments_payout_failed.subject".to_string(),
                body_key: "emails.payments_payout_failed.body".to_string(),
                args: vec![
                    ("payout".to_string(), IntentArg::from(payout)),
                    ("amount".to_string(), IntentArg::from(amount_str)),
                ],
            })
        }
        "payments.tax_compliance_lapsed" => {
            let detail = envelope
                .payload
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            Some(NotificationIntent {
                audience,
                purpose: "tax_lapsed",
                subject_key: "emails.payments_tax_lapsed.subject".to_string(),
                body_key: "emails.payments_tax_lapsed.body".to_string(),
                args: vec![("detail".to_string(), IntentArg::from(detail))],
            })
        }
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
    use crate::domain::connection::ConnectionId;
    use crate::domain::dispute::DisputeId;
    use crate::domain::events::{EventAmount, PaymentsEvent};
    use crate::domain::payment::PaymentId;
    use crate::domain::payout::PayoutId;
    use chrono::Utc;
    use sanvi_kernel::{EventId, SnowflakeId};

    #[test]
    fn contract_maps_connection_activated() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let connection_id = ConnectionId::from_snowflake(SnowflakeId::try_from(200).unwrap());
        let event = PaymentsEvent::ProviderConnectionActivated {
            connection_id,
            provider: "stripe".to_string(),
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::TenantRoles {
                tenant_id,
                roles: PAYMENTS_ROLES,
            }
        );
        assert_eq!(intent.purpose, "connection_activated");
        assert_eq!(
            intent.subject_key,
            "emails.payments_connection_activated.subject"
        );
        assert_eq!(
            intent.args,
            vec![(
                "connection".to_string(),
                IntentArg::from(connection_id.to_string())
            )]
        );
    }

    #[test]
    fn contract_maps_connection_restricted() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let connection_id = ConnectionId::from_snowflake(SnowflakeId::try_from(200).unwrap());
        let event = PaymentsEvent::ProviderConnectionRestricted {
            connection_id,
            provider: "stripe".to_string(),
            detail: "identity_verification_pending".to_string(),
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(intent.purpose, "connection_restricted");
        assert_eq!(
            intent.args,
            vec![
                (
                    "connection".to_string(),
                    IntentArg::from(connection_id.to_string())
                ),
                (
                    "detail".to_string(),
                    IntentArg::from("identity_verification_pending")
                )
            ]
        );
    }

    #[test]
    fn contract_maps_requirement_deadline_approaching() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let connection_id = ConnectionId::from_snowflake(SnowflakeId::try_from(200).unwrap());
        let deadline = Utc::now();
        let event = PaymentsEvent::RequirementDeadlineApproaching {
            connection_id,
            provider: "stripe".to_string(),
            deadline,
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(intent.purpose, "requirement_deadline");
        assert_eq!(
            intent.subject_key,
            "emails.payments_requirement_deadline.subject"
        );
        assert_eq!(
            intent.args,
            vec![
                (
                    "connection".to_string(),
                    IntentArg::from(connection_id.to_string())
                ),
                ("date".to_string(), IntentArg::from(deadline))
            ]
        );
    }

    #[test]
    fn contract_maps_dispute_opened_with_and_without_due_by() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let dispute_id = DisputeId::from_snowflake(SnowflakeId::try_from(300).unwrap());
        let payment_id = PaymentId::from_snowflake(SnowflakeId::try_from(400).unwrap());
        let amount = EventAmount {
            amount_minor: 5000,
            currency: "USD".to_string(),
        };

        // Without due_by
        let event = PaymentsEvent::DisputeOpened {
            dispute_id,
            payment_id,
            provider: "stripe".to_string(),
            amount: amount.clone(),
            due_by: None,
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.body_key, "emails.payments_dispute_opened.body");
        assert_eq!(
            intent.args,
            vec![(
                "dispute".to_string(),
                IntentArg::from(dispute_id.to_string())
            )]
        );

        // With due_by
        let due = Utc::now();
        let event_due = PaymentsEvent::DisputeOpened {
            dispute_id,
            payment_id,
            provider: "stripe".to_string(),
            amount,
            due_by: Some(due),
        };
        let envelope_due = event_due.envelope(event_id, tenant_id);
        let intent_due = notification_intent(&envelope_due).expect("intent mapped");
        assert_eq!(
            intent_due.body_key,
            "emails.payments_dispute_opened.body_due"
        );
        assert_eq!(
            intent_due.args,
            vec![
                (
                    "dispute".to_string(),
                    IntentArg::from(dispute_id.to_string())
                ),
                ("due".to_string(), IntentArg::from(due))
            ]
        );
    }

    #[test]
    fn contract_maps_payout_failed() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let payout_id = PayoutId::from_snowflake(SnowflakeId::try_from(500).unwrap());
        let connection_id = ConnectionId::from_snowflake(SnowflakeId::try_from(200).unwrap());
        let event = PaymentsEvent::PayoutFailed {
            payout_id,
            connection_id,
            provider: "stripe".to_string(),
            external_payout_id: "po_failed_123".to_string(),
            amount: EventAmount {
                amount_minor: 12500,
                currency: "USD".to_string(),
            },
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(intent.purpose, "payout_failed");
        assert_eq!(intent.subject_key, "emails.payments_payout_failed.subject");
        assert_eq!(
            intent.args,
            vec![
                ("payout".to_string(), IntentArg::from("po_failed_123")),
                ("amount".to_string(), IntentArg::from("12500 USD"))
            ]
        );
    }

    #[test]
    fn contract_maps_tax_compliance_lapsed() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = PaymentsEvent::TaxComplianceLapsed {
            tenant_id,
            detail: "acct_123".to_string(),
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(intent.purpose, "tax_lapsed");
        assert_eq!(intent.subject_key, "emails.payments_tax_lapsed.subject");
        assert_eq!(
            intent.args,
            vec![("detail".to_string(), IntentArg::from("acct_123"))]
        );
    }
}
