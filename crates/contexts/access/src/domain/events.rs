//! Domain events emitted by the access context: feature catalog changes,
//! entitlement grants/revocations/expiry, quota breaches, and role
//! authoring. Tenant-scoped events carry the tenant id in envelope metadata
//! so consumers (cache invalidation, Keto sync) can route without parsing
//! payloads.

use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use serde::Serialize;
use uuid::Uuid;

/// Machine-readable event names, stable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessEventName {
    FeatureDefined,
    EntitlementGranted,
    EntitlementRevoked,
    EntitlementExpired,
    QuotaExceeded,
    RoleCreated,
    RoleUpdated,
    RoleDeleted,
}

impl AccessEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FeatureDefined => "access.feature_defined",
            Self::EntitlementGranted => "access.entitlement_granted",
            Self::EntitlementRevoked => "access.entitlement_revoked",
            Self::EntitlementExpired => "access.entitlement_expired",
            Self::QuotaExceeded => "access.quota_exceeded",
            Self::RoleCreated => "access.role_created",
            Self::RoleUpdated => "access.role_updated",
            Self::RoleDeleted => "access.role_deleted",
        }
    }

    pub fn aggregate(self) -> &'static str {
        match self {
            Self::FeatureDefined => "access.feature",
            Self::EntitlementGranted | Self::EntitlementRevoked | Self::EntitlementExpired => {
                "access.entitlement"
            }
            Self::QuotaExceeded => "access.quota",
            Self::RoleCreated | Self::RoleUpdated | Self::RoleDeleted => "access.role",
        }
    }
}

/// Events the access context publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccessEvent {
    FeatureDefined {
        key: String,
    },
    EntitlementGranted {
        tenant_id: Uuid,
        feature_key: String,
        enabled: bool,
        limit: Option<i64>,
        expires_at: Option<DateTime<Utc>>,
    },
    EntitlementRevoked {
        tenant_id: Uuid,
        feature_key: String,
    },
    EntitlementExpired {
        tenant_id: Uuid,
        feature_key: String,
    },
    QuotaExceeded {
        tenant_id: Uuid,
        feature_key: String,
        limit: i64,
        attempted: i64,
    },
    RoleCreated {
        tenant_id: Option<Uuid>,
        role_id: Uuid,
        key: String,
        permissions: Vec<String>,
    },
    RoleUpdated {
        tenant_id: Option<Uuid>,
        role_id: Uuid,
        key: String,
        added_permissions: Vec<String>,
        removed_permissions: Vec<String>,
    },
    RoleDeleted {
        tenant_id: Option<Uuid>,
        role_id: Uuid,
        key: String,
        permissions: Vec<String>,
    },
}

impl AccessEvent {
    pub fn name(&self) -> AccessEventName {
        match self {
            Self::FeatureDefined { .. } => AccessEventName::FeatureDefined,
            Self::EntitlementGranted { .. } => AccessEventName::EntitlementGranted,
            Self::EntitlementRevoked { .. } => AccessEventName::EntitlementRevoked,
            Self::EntitlementExpired { .. } => AccessEventName::EntitlementExpired,
            Self::QuotaExceeded { .. } => AccessEventName::QuotaExceeded,
            Self::RoleCreated { .. } => AccessEventName::RoleCreated,
            Self::RoleUpdated { .. } => AccessEventName::RoleUpdated,
            Self::RoleDeleted { .. } => AccessEventName::RoleDeleted,
        }
    }

    /// The tenant the event concerns, when it concerns one.
    pub fn tenant_id(&self) -> Option<Uuid> {
        match self {
            Self::EntitlementGranted { tenant_id, .. }
            | Self::EntitlementRevoked { tenant_id, .. }
            | Self::EntitlementExpired { tenant_id, .. }
            | Self::QuotaExceeded { tenant_id, .. } => Some(*tenant_id),
            Self::RoleCreated { tenant_id, .. }
            | Self::RoleUpdated { tenant_id, .. }
            | Self::RoleDeleted { tenant_id, .. } => *tenant_id,
            Self::FeatureDefined { .. } => None,
        }
    }

    /// The outbox envelope for this event. Tenant-scoped events tag the
    /// tenant id in metadata; the aggregate id is the role id (role
    /// events), the tenant id (entitlement/quota events) or nil (feature
    /// events, which have no natural uuid).
    pub fn envelope(&self) -> EventEnvelope {
        let payload = serde_json::to_value(self).expect("access events serialize infallibly");
        let aggregate_id = match self {
            Self::RoleCreated { role_id, .. }
            | Self::RoleUpdated { role_id, .. }
            | Self::RoleDeleted { role_id, .. } => *role_id,
            Self::EntitlementGranted { tenant_id, .. }
            | Self::EntitlementRevoked { tenant_id, .. }
            | Self::EntitlementExpired { tenant_id, .. }
            | Self::QuotaExceeded { tenant_id, .. } => *tenant_id,
            Self::FeatureDefined { .. } => Uuid::nil(),
        };
        let envelope = EventEnvelope::new(
            self.name().aggregate(),
            aggregate_id,
            self.name().as_str(),
            payload,
        );
        match self.tenant_id() {
            Some(tenant_id) => {
                envelope.with_metadata("tenant_id", serde_json::json!(tenant_id.to_string()))
            }
            None => envelope,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_and_aggregates_are_stable() {
        assert_eq!(
            AccessEventName::FeatureDefined.as_str(),
            "access.feature_defined"
        );
        assert_eq!(
            AccessEventName::EntitlementGranted.aggregate(),
            "access.entitlement"
        );
        assert_eq!(AccessEventName::RoleCreated.aggregate(), "access.role");
    }

    #[test]
    fn tenant_scoped_events_carry_tenant_metadata() {
        let tenant_id = Uuid::now_v7();
        let event = AccessEvent::EntitlementGranted {
            tenant_id,
            feature_key: "domains.custom".to_string(),
            enabled: true,
            limit: None,
            expires_at: None,
        };
        let envelope = event.envelope();
        assert_eq!(envelope.metadata["tenant_id"], tenant_id.to_string());
        assert_eq!(envelope.aggregate_id, tenant_id);
    }

    #[test]
    fn role_events_use_the_role_id_as_aggregate_id() {
        let role_id = Uuid::now_v7();
        let event = AccessEvent::RoleCreated {
            tenant_id: Some(Uuid::now_v7()),
            role_id,
            key: "support_agent".to_string(),
            permissions: vec!["identity.member.read".to_string()],
        };
        let envelope = event.envelope();
        assert_eq!(envelope.aggregate_id, role_id);
        assert!(envelope.metadata["tenant_id"].is_string());
    }
}
