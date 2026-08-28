use chrono::{DateTime, Utc};
use serde::Serialize;

use sanvi_kernel::{EventEnvelope, EventId, SnowflakeId, TenantId};

use crate::domain::{CustomDomainId, DomainOrderId};

pub const DOMAIN_AGGREGATE: &str = "domains.domain";
pub const ORDER_AGGREGATE: &str = "domains.order";
pub const CERTIFICATE_AGGREGATE: &str = "domains.certificate";
pub const DEFAULT_LOCALE: &str = "en";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainsEventName {
    DomainClaimed,
    DomainVerified,
    DomainVerificationFailed,
    DomainWentLive,
    DomainDegraded,
    DomainRemoved,
    CertificateIssued,
    CertificateRenewed,
    CertificateRenewalFailed,
    DomainOrderPlaced,
    DomainOrderCompleted,
    DomainOrderFailed,
    DomainExpiringSoon,
}

impl DomainsEventName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DomainClaimed => "domains.domain_claimed",
            Self::DomainVerified => "domains.domain_verified",
            Self::DomainVerificationFailed => "domains.domain_verification_failed",
            Self::DomainWentLive => "domains.domain_went_live",
            Self::DomainDegraded => "domains.domain_degraded",
            Self::DomainRemoved => "domains.domain_removed",
            Self::CertificateIssued => "domains.certificate_issued",
            Self::CertificateRenewed => "domains.certificate_renewed",
            Self::CertificateRenewalFailed => "domains.certificate_renewal_failed",
            Self::DomainOrderPlaced => "domains.domain_order_placed",
            Self::DomainOrderCompleted => "domains.domain_order_completed",
            Self::DomainOrderFailed => "domains.domain_order_failed",
            Self::DomainExpiringSoon => "domains.domain_expiring_soon",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "domains.domain_claimed" => Self::DomainClaimed,
            "domains.domain_verified" => Self::DomainVerified,
            "domains.domain_verification_failed" => Self::DomainVerificationFailed,
            "domains.domain_went_live" => Self::DomainWentLive,
            "domains.domain_degraded" => Self::DomainDegraded,
            "domains.domain_removed" => Self::DomainRemoved,
            "domains.certificate_issued" => Self::CertificateIssued,
            "domains.certificate_renewed" => Self::CertificateRenewed,
            "domains.certificate_renewal_failed" => Self::CertificateRenewalFailed,
            "domains.domain_order_placed" => Self::DomainOrderPlaced,
            "domains.domain_order_completed" => Self::DomainOrderCompleted,
            "domains.domain_order_failed" => Self::DomainOrderFailed,
            "domains.domain_expiring_soon" => Self::DomainExpiringSoon,
            _ => return None,
        })
    }
}

/// The event payloads, internally tagged.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DomainsEvent {
    DomainClaimed {
        domain_id: CustomDomainId,
        hostname: String,
        domain_kind: String,
        role: String,
    },
    DomainVerified {
        domain_id: CustomDomainId,
        hostname: String,
    },
    DomainVerificationFailed {
        domain_id: CustomDomainId,
        hostname: String,
        code: String,
        detail: String,
    },
    DomainWentLive {
        domain_id: CustomDomainId,
        hostname: String,
    },
    DomainDegraded {
        domain_id: CustomDomainId,
        hostname: String,
        code: String,
        detail: String,
    },
    DomainRemoved {
        domain_id: CustomDomainId,
        hostname: String,
        reason: String,
    },
    CertificateIssued {
        domain_id: CustomDomainId,
        hostname: String,
        not_after: DateTime<Utc>,
    },
    CertificateRenewed {
        domain_id: CustomDomainId,
        hostname: String,
        not_after: DateTime<Utc>,
    },
    CertificateRenewalFailed {
        domain_id: CustomDomainId,
        hostname: String,
        not_after: DateTime<Utc>,
        detail: String,
    },
    DomainOrderPlaced {
        order_id: DomainOrderId,
        hostname: String,
        term_years: u8,
        price_minor: i64,
        currency: String,
    },
    DomainOrderCompleted {
        order_id: DomainOrderId,
        hostname: String,
    },
    DomainOrderFailed {
        order_id: DomainOrderId,
        hostname: String,
        detail: String,
    },
    DomainExpiringSoon {
        domain_id: CustomDomainId,
        hostname: String,
        days_left: u32,
    },
}

impl DomainsEvent {
    pub fn name(&self) -> DomainsEventName {
        match self {
            Self::DomainClaimed { .. } => DomainsEventName::DomainClaimed,
            Self::DomainVerified { .. } => DomainsEventName::DomainVerified,
            Self::DomainVerificationFailed { .. } => DomainsEventName::DomainVerificationFailed,
            Self::DomainWentLive { .. } => DomainsEventName::DomainWentLive,
            Self::DomainDegraded { .. } => DomainsEventName::DomainDegraded,
            Self::DomainRemoved { .. } => DomainsEventName::DomainRemoved,
            Self::CertificateIssued { .. } => DomainsEventName::CertificateIssued,
            Self::CertificateRenewed { .. } => DomainsEventName::CertificateRenewed,
            Self::CertificateRenewalFailed { .. } => DomainsEventName::CertificateRenewalFailed,
            Self::DomainOrderPlaced { .. } => DomainsEventName::DomainOrderPlaced,
            Self::DomainOrderCompleted { .. } => DomainsEventName::DomainOrderCompleted,
            Self::DomainOrderFailed { .. } => DomainsEventName::DomainOrderFailed,
            Self::DomainExpiringSoon { .. } => DomainsEventName::DomainExpiringSoon,
        }
    }

    /// The aggregate id (domain id, order id, or certificate's domain id).
    pub fn aggregate_id(&self) -> SnowflakeId {
        match self {
            Self::DomainClaimed { domain_id, .. }
            | Self::DomainVerified { domain_id, .. }
            | Self::DomainVerificationFailed { domain_id, .. }
            | Self::DomainWentLive { domain_id, .. }
            | Self::DomainDegraded { domain_id, .. }
            | Self::DomainRemoved { domain_id, .. }
            | Self::CertificateIssued { domain_id, .. }
            | Self::CertificateRenewed { domain_id, .. }
            | Self::CertificateRenewalFailed { domain_id, .. }
            | Self::DomainExpiringSoon { domain_id, .. } => domain_id.as_snowflake(),
            Self::DomainOrderPlaced { order_id, .. }
            | Self::DomainOrderCompleted { order_id, .. }
            | Self::DomainOrderFailed { order_id, .. } => order_id.as_snowflake(),
        }
    }

    /// The hostname the event is about (cache invalidation key).
    pub fn hostname(&self) -> &str {
        match self {
            Self::DomainClaimed { hostname, .. }
            | Self::DomainVerified { hostname, .. }
            | Self::DomainVerificationFailed { hostname, .. }
            | Self::DomainWentLive { hostname, .. }
            | Self::DomainDegraded { hostname, .. }
            | Self::DomainRemoved { hostname, .. }
            | Self::CertificateIssued { hostname, .. }
            | Self::CertificateRenewed { hostname, .. }
            | Self::CertificateRenewalFailed { hostname, .. }
            | Self::DomainExpiringSoon { hostname, .. }
            | Self::DomainOrderPlaced { hostname, .. }
            | Self::DomainOrderCompleted { hostname, .. }
            | Self::DomainOrderFailed { hostname, .. } => hostname,
        }
    }

    pub fn envelope(&self, id: EventId, tenant_id: TenantId) -> EventEnvelope {
        let aggregate = match self {
            Self::DomainOrderPlaced { .. }
            | Self::DomainOrderCompleted { .. }
            | Self::DomainOrderFailed { .. } => ORDER_AGGREGATE,
            Self::CertificateIssued { .. }
            | Self::CertificateRenewed { .. }
            | Self::CertificateRenewalFailed { .. } => CERTIFICATE_AGGREGATE,
            _ => DOMAIN_AGGREGATE,
        };
        EventEnvelope::new(
            id,
            aggregate,
            self.aggregate_id(),
            self.name().as_str(),
            serde_json::to_value(self).expect("domains events serialize"),
        )
        .with_metadata("tenant_id", serde_json::json!(tenant_id.to_string()))
        .with_locale(DEFAULT_LOCALE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for event in [
            DomainsEvent::DomainClaimed {
                domain_id: CustomDomainId::try_from(1i64).unwrap(),
                hostname: "example.com".to_string(),
                domain_kind: "connected".to_string(),
                role: "primary".to_string(),
            },
            DomainsEvent::DomainOrderFailed {
                order_id: DomainOrderId::try_from(2i64).unwrap(),
                hostname: "example.com".to_string(),
                detail: "charge failed".to_string(),
            },
        ] {
            let parsed = DomainsEventName::parse(event.name().as_str());
            assert_eq!(parsed, Some(event.name()));
        }
    }

    #[test]
    fn envelope_carries_tenant_metadata() {
        let event = DomainsEvent::DomainVerified {
            domain_id: CustomDomainId::try_from(1i64).unwrap(),
            hostname: "example.com".to_string(),
        };
        let tenant_id = TenantId::try_from(42i64).unwrap();
        let event_id = EventId::try_from(100i64).unwrap();
        let envelope = event.envelope(event_id, tenant_id);
        assert_eq!(envelope.event_type, "domains.domain_verified");
        assert_eq!(
            envelope.metadata.get("tenant_id").and_then(|v| v.as_str()),
            Some(tenant_id.to_string().as_str())
        );
    }
}
