//! Maps committed advertising events to notification intents.

use sanvi_kernel::EventEnvelope;
use sanvi_kernel::TenantId;
use sanvi_kernel::notifications::{Audience, IntentArg, NotificationIntent};

const ADVERTISING_ROLES: &[&str] = &["tenant_owner", "tenant_admin"];

pub fn notification_intent(envelope: &EventEnvelope) -> Option<NotificationIntent> {
    let tenant_id = extract_tenant_id(envelope)?;
    let audience = Audience::TenantRoles {
        tenant_id,
        roles: ADVERTISING_ROLES,
    };

    if envelope.event_type.as_str() == "advertising.budget_guardrails_suspended" {
        let payload = envelope.payload.get("payload").unwrap_or(&envelope.payload);
        let tenant_name = payload
            .get("tenant_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        return Some(NotificationIntent {
            audience,
            purpose: "budget_guardrails_rollback",
            subject_key: "emails.budget_guardrails_rollback.subject".to_string(),
            body_key: "emails.budget_guardrails_rollback.body".to_string(),
            args: vec![("tenant".to_string(), IntentArg::from(tenant_name))],
        });
    }

    if envelope.event_type.as_str() != "advertising.spend_threshold_reached" {
        return None;
    }

    let payload = envelope.payload.get("payload").unwrap_or(&envelope.payload);

    let scope_name = payload
        .get("scope_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let spend_minor = payload.get("spend_minor").and_then(|v| v.as_i64());
    let currency = payload
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("USD");
    let cap_minor = payload.get("cap_minor").and_then(|v| v.as_i64());
    let period = payload
        .get("period")
        .and_then(|v| v.as_str())
        .unwrap_or("monthly");
    let action_taken = payload
        .get("action_taken")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let condition = payload.get("condition").and_then(|v| v.as_str());
    let threshold = payload
        .get("threshold")
        .and_then(|v| v.as_u64())
        .unwrap_or(100);

    let spend_str = match spend_minor {
        Some(minor) => format!("{minor} {currency}"),
        None => String::new(),
    };
    let cap_str = match cap_minor {
        Some(minor) => format!("{minor} {currency}"),
        None => String::new(),
    };

    match condition {
        Some("threshold_80") => Some(NotificationIntent {
            audience,
            purpose: "budget_warning_80",
            subject_key: "emails.budget_warning_80.subject".to_string(),
            body_key: "emails.budget_warning_80.body".to_string(),
            args: vec![
                ("scope_name".to_string(), IntentArg::from(scope_name)),
                ("spend".to_string(), IntentArg::from(spend_str)),
                ("cap_amount".to_string(), IntentArg::from(cap_str)),
                ("period".to_string(), IntentArg::from(period)),
            ],
        }),
        Some("settled_breach") | Some("provisional_lower_bound_breach") => {
            Some(NotificationIntent {
                audience,
                purpose: "budget_cap_breached",
                subject_key: "emails.budget_cap_breached.subject".to_string(),
                body_key: "emails.budget_cap_breached.body".to_string(),
                args: vec![
                    ("scope_name".to_string(), IntentArg::from(scope_name)),
                    ("spend".to_string(), IntentArg::from(spend_str)),
                    ("period".to_string(), IntentArg::from(period)),
                    ("cap_amount".to_string(), IntentArg::from(cap_str)),
                    ("action_taken".to_string(), IntentArg::from(action_taken)),
                ],
            })
        }
        Some("provisional_warning") => Some(NotificationIntent {
            audience,
            purpose: "budget_provisional_warning",
            subject_key: "emails.budget_provisional_warning.subject".to_string(),
            body_key: "emails.budget_provisional_warning.body".to_string(),
            args: vec![
                ("scope_name".to_string(), IntentArg::from(scope_name)),
                ("spend".to_string(), IntentArg::from(spend_str)),
                ("period".to_string(), IntentArg::from(period)),
                ("cap_amount".to_string(), IntentArg::from(cap_str)),
                ("lower_bound".to_string(), IntentArg::from("")),
            ],
        }),
        Some("stale_warning") => Some(NotificationIntent {
            audience,
            purpose: "budget_stale_warning",
            subject_key: "emails.budget_stale_warning.subject".to_string(),
            body_key: "emails.budget_stale_warning.body".to_string(),
            args: vec![
                ("scope_name".to_string(), IntentArg::from(scope_name)),
                ("lag_hours".to_string(), IntentArg::from("0")),
            ],
        }),
        Some("spend_anomaly") => Some(NotificationIntent {
            audience,
            purpose: "budget_spend_anomaly",
            subject_key: "emails.budget_spend_anomaly.subject".to_string(),
            body_key: "emails.budget_spend_anomaly.body".to_string(),
            args: vec![
                ("scope_name".to_string(), IntentArg::from(scope_name)),
                ("spend".to_string(), IntentArg::from(spend_str)),
                ("average_spend".to_string(), IntentArg::from("")),
                ("sigma".to_string(), IntentArg::from("3")),
            ],
        }),
        _ => {
            if threshold == 80 {
                Some(NotificationIntent {
                    audience,
                    purpose: "budget_warning_80",
                    subject_key: "emails.budget_warning_80.subject".to_string(),
                    body_key: "emails.budget_warning_80.body".to_string(),
                    args: vec![
                        ("scope_name".to_string(), IntentArg::from(scope_name)),
                        ("spend".to_string(), IntentArg::from(spend_str)),
                        ("cap_amount".to_string(), IntentArg::from(cap_str)),
                        ("period".to_string(), IntentArg::from(period)),
                    ],
                })
            } else {
                Some(NotificationIntent {
                    audience,
                    purpose: "budget_cap_breached",
                    subject_key: "emails.budget_cap_breached.subject".to_string(),
                    body_key: "emails.budget_cap_breached.body".to_string(),
                    args: vec![
                        ("scope_name".to_string(), IntentArg::from(scope_name)),
                        ("spend".to_string(), IntentArg::from(spend_str)),
                        ("period".to_string(), IntentArg::from(period)),
                        ("cap_amount".to_string(), IntentArg::from(cap_str)),
                        ("action_taken".to_string(), IntentArg::from(action_taken)),
                    ],
                })
            }
        }
    }
}

fn extract_tenant_id(envelope: &EventEnvelope) -> Option<TenantId> {
    envelope
        .metadata
        .get("tenant_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<TenantId>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AdEvent;
    use crate::domain::model::{AlertCondition, BudgetAlertId, BudgetPeriod};
    use sanvi_kernel::{EventId, SnowflakeId};

    fn test_envelope(event: AdEvent, tenant_id: TenantId) -> EventEnvelope {
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        event.envelope(tenant_id, event_id)
    }

    #[test]
    fn contract_maps_threshold_80() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event = AdEvent::SpendThresholdReached {
            campaign_id: None,
            threshold: 80,
            alert_id: Some(BudgetAlertId::from_snowflake(
                SnowflakeId::try_from(50).unwrap(),
            )),
            scope_name: Some("All campaigns".to_string()),
            spend_minor: Some(8000),
            currency: Some("USD".to_string()),
            cap_minor: Some(10000),
            period: Some(BudgetPeriod::Monthly),
            action_taken: None,
            condition: Some(AlertCondition::Threshold80),
        };
        let envelope = test_envelope(event, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::TenantRoles {
                tenant_id,
                roles: ADVERTISING_ROLES,
            }
        );
        assert_eq!(intent.purpose, "budget_warning_80");
        assert_eq!(intent.subject_key, "emails.budget_warning_80.subject");
        assert_eq!(intent.body_key, "emails.budget_warning_80.body");
        assert_eq!(
            intent.args,
            vec![
                (
                    "scope_name".to_string(),
                    IntentArg::from("All campaigns".to_string())
                ),
                ("spend".to_string(), IntentArg::from("8000 USD".to_string())),
                (
                    "cap_amount".to_string(),
                    IntentArg::from("10000 USD".to_string())
                ),
                ("period".to_string(), IntentArg::from("monthly".to_string())),
            ]
        );
    }

    #[test]
    fn contract_maps_cap_breached() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event = AdEvent::SpendThresholdReached {
            campaign_id: None,
            threshold: 100,
            alert_id: Some(BudgetAlertId::from_snowflake(
                SnowflakeId::try_from(51).unwrap(),
            )),
            scope_name: Some("All campaigns".to_string()),
            spend_minor: Some(10500),
            currency: Some("USD".to_string()),
            cap_minor: Some(10000),
            period: Some(BudgetPeriod::Monthly),
            action_taken: Some("Active campaigns were automatically paused.".to_string()),
            condition: Some(AlertCondition::SettledBreach),
        };
        let envelope = test_envelope(event, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(intent.purpose, "budget_cap_breached");
        assert_eq!(intent.subject_key, "emails.budget_cap_breached.subject");
        assert_eq!(intent.body_key, "emails.budget_cap_breached.body");
        assert_eq!(
            intent.args,
            vec![
                (
                    "scope_name".to_string(),
                    IntentArg::from("All campaigns".to_string())
                ),
                (
                    "spend".to_string(),
                    IntentArg::from("10500 USD".to_string())
                ),
                ("period".to_string(), IntentArg::from("monthly".to_string())),
                (
                    "cap_amount".to_string(),
                    IntentArg::from("10000 USD".to_string())
                ),
                (
                    "action_taken".to_string(),
                    IntentArg::from("Active campaigns were automatically paused.".to_string())
                ),
            ]
        );
    }

    #[test]
    fn contract_maps_spend_anomaly() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event = AdEvent::SpendThresholdReached {
            campaign_id: None,
            threshold: 100,
            alert_id: Some(BudgetAlertId::from_snowflake(
                SnowflakeId::try_from(52).unwrap(),
            )),
            scope_name: Some("Campaign X".to_string()),
            spend_minor: Some(50000),
            currency: Some("JPY".to_string()),
            cap_minor: Some(0),
            period: None,
            action_taken: None,
            condition: Some(AlertCondition::SpendAnomaly),
        };
        let envelope = test_envelope(event, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(intent.purpose, "budget_spend_anomaly");
        assert_eq!(intent.subject_key, "emails.budget_spend_anomaly.subject");
        assert_eq!(intent.body_key, "emails.budget_spend_anomaly.body");
    }

    #[test]
    fn contract_maps_guardrails_suspended_to_rollback_notice() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event = AdEvent::BudgetGuardrailsSuspended {
            tenant_name: "acme".to_string(),
            reason: "feature_disabled".to_string(),
        };
        let envelope = test_envelope(event, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(intent.purpose, "budget_guardrails_rollback");
        assert_eq!(
            intent.subject_key,
            "emails.budget_guardrails_rollback.subject"
        );
        assert_eq!(intent.body_key, "emails.budget_guardrails_rollback.body");
        assert_eq!(
            intent.args,
            vec![("tenant".to_string(), IntentArg::from("acme".to_string()))]
        );
    }

    #[test]
    fn contract_ignores_other_events() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event = AdEvent::AccountDisconnected {
            connection_id: crate::domain::model::ConnectionId::from_snowflake(
                SnowflakeId::try_from(10).unwrap(),
            ),
        };
        let envelope = test_envelope(event, tenant_id);
        assert!(notification_intent(&envelope).is_none());
    }
}
