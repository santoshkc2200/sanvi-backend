//! Domain events emitted by the tenant aggregate.
//!
//! Every event carries the tenant id in its envelope metadata so consumers
//! (cache invalidation, audit fan-out) can route without parsing payloads.

use sanvi_kernel::EventEnvelope;
use serde::Serialize;

use crate::domain::tenant::TenantId;
use crate::domain::values::{DataRegion, SuspensionReason, TenantSlug};

/// Machine-readable event names, stable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantEventName {
    SlugReserved,
    Provisioned,
    Activated,
    Suspended,
    Resumed,
    Archived,
    PurgeRequested,
    Purged,
    SettingsChanged,
}

impl TenantEventName {
    pub const AGGREGATE: &'static str = "tenancy.tenant";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SlugReserved => "tenancy.tenant_slug_reserved",
            Self::Provisioned => "tenancy.tenant_provisioned",
            Self::Activated => "tenancy.tenant_activated",
            Self::Suspended => "tenancy.tenant_suspended",
            Self::Resumed => "tenancy.tenant_resumed",
            Self::Archived => "tenancy.tenant_archived",
            Self::PurgeRequested => "tenancy.tenant_purge_requested",
            Self::Purged => "tenancy.tenant_purged",
            Self::SettingsChanged => "tenancy.tenant_settings_changed",
        }
    }
}

/// Events the tenancy context publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TenantEvent {
    SlugReserved {
        tenant_id: TenantId,
        slug: TenantSlug,
    },
    Provisioned {
        tenant_id: TenantId,
        slug: TenantSlug,
        region: DataRegion,
    },
    Activated {
        tenant_id: TenantId,
        slug: TenantSlug,
    },
    Suspended {
        tenant_id: TenantId,
        slug: TenantSlug,
        reason: SuspensionReason,
    },
    Resumed {
        tenant_id: TenantId,
        slug: TenantSlug,
    },
    Archived {
        tenant_id: TenantId,
        slug: TenantSlug,
    },
    PurgeRequested {
        tenant_id: TenantId,
        slug: TenantSlug,
    },
    Purged {
        tenant_id: TenantId,
        slug: TenantSlug,
    },
    SettingsChanged {
        tenant_id: TenantId,
        key: String,
        value: serde_json::Value,
    },
}

impl TenantEvent {
    pub fn name(&self) -> TenantEventName {
        match self {
            Self::SlugReserved { .. } => TenantEventName::SlugReserved,
            Self::Provisioned { .. } => TenantEventName::Provisioned,
            Self::Activated { .. } => TenantEventName::Activated,
            Self::Suspended { .. } => TenantEventName::Suspended,
            Self::Resumed { .. } => TenantEventName::Resumed,
            Self::Archived { .. } => TenantEventName::Archived,
            Self::PurgeRequested { .. } => TenantEventName::PurgeRequested,
            Self::Purged { .. } => TenantEventName::Purged,
            Self::SettingsChanged { .. } => TenantEventName::SettingsChanged,
        }
    }

    pub fn tenant_id(&self) -> TenantId {
        match self {
            Self::SlugReserved { tenant_id, .. }
            | Self::Provisioned { tenant_id, .. }
            | Self::Activated { tenant_id, .. }
            | Self::Suspended { tenant_id, .. }
            | Self::Resumed { tenant_id, .. }
            | Self::Archived { tenant_id, .. }
            | Self::PurgeRequested { tenant_id, .. }
            | Self::Purged { tenant_id, .. }
            | Self::SettingsChanged { tenant_id, .. } => *tenant_id,
        }
    }

    /// The outbox envelope for this event, tagged with the tenant id.
    pub fn envelope(&self) -> EventEnvelope {
        let payload = serde_json::to_value(self).expect("tenant events serialize infallibly");
        EventEnvelope::new(
            TenantEventName::AGGREGATE,
            self.tenant_id().as_uuid(),
            self.name().as_str(),
            payload,
        )
        .with_metadata(
            "tenant_id",
            serde_json::json!(self.tenant_id().as_uuid().to_string()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_carries_tenant_id_metadata() {
        let events: Vec<TenantEvent> = vec![
            TenantEvent::SlugReserved {
                tenant_id: TenantId::new(),
                slug: TenantSlug::parse("acme").unwrap(),
            },
            TenantEvent::SettingsChanged {
                tenant_id: TenantId::new(),
                key: "theme".to_string(),
                value: serde_json::json!({ "primary": "#000" }),
            },
        ];
        for event in events {
            let envelope = event.envelope();
            assert_eq!(envelope.aggregate, "tenancy.tenant");
            assert_eq!(
                envelope.metadata["tenant_id"],
                envelope.aggregate_id.to_string()
            );
            assert!(envelope.event_type.starts_with("tenancy.tenant_"));
        }
    }

    #[test]
    fn event_names_are_stable() {
        assert_eq!(
            TenantEventName::Provisioned.as_str(),
            "tenancy.tenant_provisioned"
        );
        assert_eq!(
            TenantEventName::SettingsChanged.as_str(),
            "tenancy.tenant_settings_changed"
        );
    }
}
