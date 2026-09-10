//! Maps committed domain events to notification intents.

use sanvi_kernel::EventEnvelope;
use sanvi_kernel::TenantId;
use sanvi_kernel::notifications::{Audience, IntentArg, NotificationIntent};

const DOMAIN_ROLES: &[&str] = &["tenant_owner", "tenant_admin"];

pub fn notification_intent(envelope: &EventEnvelope) -> Option<NotificationIntent> {
    let tenant_id = extract_tenant_id(envelope)?;
    let audience = Audience::TenantRoles {
        tenant_id,
        roles: DOMAIN_ROLES,
    };

    match envelope.event_type.as_str() {
        "domains.domain_degraded" => {
            let hostname = envelope.payload.get("hostname")?.as_str()?.to_string();
            let code = envelope
                .payload
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let detail = envelope
                .payload
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            Some(NotificationIntent {
                audience,
                purpose: "domain_degraded",
                subject_key: "emails.domain_degraded.subject".to_string(),
                body_key: "emails.domain_degraded.body".to_string(),
                args: vec![
                    ("hostname".to_string(), IntentArg::from(hostname)),
                    ("code".to_string(), IntentArg::from(code)),
                    ("detail".to_string(), IntentArg::from(detail)),
                ],
            })
        }
        "domains.certificate_renewal_failed" => {
            let hostname = envelope.payload.get("hostname")?.as_str()?.to_string();
            let detail = envelope
                .payload
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            Some(NotificationIntent {
                audience,
                purpose: "certificate_renewal_failed",
                subject_key: "emails.certificate_renewal_failed.subject".to_string(),
                body_key: "emails.certificate_renewal_failed.body".to_string(),
                args: vec![
                    ("hostname".to_string(), IntentArg::from(hostname)),
                    ("detail".to_string(), IntentArg::from(detail)),
                ],
            })
        }
        "domains.domain_expiring_soon" => {
            let hostname = envelope.payload.get("hostname")?.as_str()?.to_string();
            let days_left = envelope.payload.get("days_left")?.as_i64()?;

            Some(NotificationIntent {
                audience,
                purpose: "domain_expiring_soon",
                subject_key: "emails.domain_expiring_soon.subject".to_string(),
                body_key: "emails.domain_expiring_soon.body".to_string(),
                args: vec![
                    ("hostname".to_string(), IntentArg::from(hostname)),
                    ("days_left".to_string(), IntentArg::from(days_left)),
                ],
            })
        }
        "domains.domain_order_failed" => {
            let hostname = envelope.payload.get("hostname")?.as_str()?.to_string();
            let detail = envelope
                .payload
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            Some(NotificationIntent {
                audience,
                purpose: "domain_order_failed",
                subject_key: "emails.domain_order_failed.subject".to_string(),
                body_key: "emails.domain_order_failed.body".to_string(),
                args: vec![
                    ("hostname".to_string(), IntentArg::from(hostname)),
                    ("detail".to_string(), IntentArg::from(detail)),
                ],
            })
        }
        "domains.domain_removed" => {
            let hostname = envelope.payload.get("hostname")?.as_str()?.to_string();
            let reason = envelope
                .payload
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            Some(NotificationIntent {
                audience,
                purpose: "domain_removed",
                subject_key: "emails.domain_removed.subject".to_string(),
                body_key: "emails.domain_removed.body".to_string(),
                args: vec![
                    ("hostname".to_string(), IntentArg::from(hostname)),
                    ("reason".to_string(), IntentArg::from(reason)),
                ],
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CustomDomainId, DomainOrderId, DomainsEvent};
    use chrono::Utc;
    use sanvi_kernel::{EventId, SnowflakeId};

    #[test]
    fn contract_maps_domain_degraded() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = DomainsEvent::DomainDegraded {
            domain_id: CustomDomainId::from_snowflake(SnowflakeId::try_from(2).unwrap()),
            hostname: "example.com".to_string(),
            code: "dns_error".to_string(),
            detail: "CNAME missing".to_string(),
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");

        assert_eq!(
            intent.audience,
            Audience::TenantRoles {
                tenant_id,
                roles: DOMAIN_ROLES,
            }
        );
        assert_eq!(intent.purpose, "domain_degraded");
        assert_eq!(intent.subject_key, "emails.domain_degraded.subject");
        assert_eq!(
            intent.args,
            vec![
                (
                    "hostname".to_string(),
                    IntentArg::Text("example.com".to_string())
                ),
                ("code".to_string(), IntentArg::Text("dns_error".to_string())),
                (
                    "detail".to_string(),
                    IntentArg::Text("CNAME missing".to_string())
                ),
            ]
        );
    }

    #[test]
    fn contract_maps_certificate_renewal_failed() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = DomainsEvent::CertificateRenewalFailed {
            domain_id: CustomDomainId::from_snowflake(SnowflakeId::try_from(2).unwrap()),
            hostname: "example.com".to_string(),
            not_after: Utc::now(),
            detail: "challenge failed".to_string(),
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "certificate_renewal_failed");
    }

    #[test]
    fn contract_maps_domain_expiring_soon() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = DomainsEvent::DomainExpiringSoon {
            domain_id: CustomDomainId::from_snowflake(SnowflakeId::try_from(2).unwrap()),
            hostname: "example.com".to_string(),
            days_left: 7,
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "domain_expiring_soon");
        assert_eq!(
            intent.args,
            vec![
                (
                    "hostname".to_string(),
                    IntentArg::Text("example.com".to_string())
                ),
                ("days_left".to_string(), IntentArg::Int(7)),
            ]
        );
    }

    #[test]
    fn contract_maps_domain_order_failed() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = DomainsEvent::DomainOrderFailed {
            order_id: DomainOrderId::from_snowflake(SnowflakeId::try_from(3).unwrap()),
            hostname: "example.com".to_string(),
            detail: "card declined".to_string(),
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "domain_order_failed");
    }

    #[test]
    fn contract_maps_domain_removed() {
        let tenant_id = TenantId::from_snowflake(SnowflakeId::try_from(100).unwrap());
        let event_id = EventId::from_snowflake(SnowflakeId::try_from(1).unwrap());
        let event = DomainsEvent::DomainRemoved {
            domain_id: CustomDomainId::from_snowflake(SnowflakeId::try_from(2).unwrap()),
            hostname: "example.com".to_string(),
            reason: "requested by user".to_string(),
        };
        let envelope = event.envelope(event_id, tenant_id);
        let intent = notification_intent(&envelope).expect("intent mapped");
        assert_eq!(intent.purpose, "domain_removed");
    }
}
