//! Domain events emitted by the platform_admin context. Every
//! tenant-scoped event carries the tenant id in envelope metadata.

use sanvi_kernel::EventEnvelope;
use serde::Serialize;
use uuid::Uuid;

use crate::domain::impersonation::{ImpersonationGrantId, ImpersonationMode};

/// Machine-readable event names, stable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformAdminEventName {
    ImpersonationStarted,
    ImpersonationEnded,
}

impl PlatformAdminEventName {
    pub const AGGREGATE: &'static str = "platform_admin.impersonation";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImpersonationStarted => "platform_admin.impersonation_started",
            Self::ImpersonationEnded => "platform_admin.impersonation_ended",
        }
    }
}

/// Events the platform_admin context publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlatformAdminEvent {
    ImpersonationStarted {
        grant_id: Uuid,
        actor_id: Uuid,
        tenant_id: Uuid,
        target_user_id: Uuid,
        mode: ImpersonationMode,
        reason: String,
        expires_at: chrono::DateTime<chrono::Utc>,
    },
    ImpersonationEnded {
        grant_id: Uuid,
        actor_id: Uuid,
        tenant_id: Uuid,
        target_user_id: Uuid,
        /// The operator who revoked (revocations only; expiry is passive).
        ended_by: Uuid,
    },
}

impl PlatformAdminEvent {
    pub fn name(&self) -> PlatformAdminEventName {
        match self {
            Self::ImpersonationStarted { .. } => PlatformAdminEventName::ImpersonationStarted,
            Self::ImpersonationEnded { .. } => PlatformAdminEventName::ImpersonationEnded,
        }
    }

    pub fn tenant_id(&self) -> Uuid {
        match self {
            Self::ImpersonationStarted { tenant_id, .. }
            | Self::ImpersonationEnded { tenant_id, .. } => *tenant_id,
        }
    }

    pub fn envelope(&self) -> EventEnvelope {
        let payload =
            serde_json::to_value(self).expect("platform admin events serialize infallibly");
        let aggregate_id = match self {
            Self::ImpersonationStarted { grant_id, .. }
            | Self::ImpersonationEnded { grant_id, .. } => *grant_id,
        };
        EventEnvelope::new(
            PlatformAdminEventName::AGGREGATE,
            aggregate_id,
            self.name().as_str(),
            payload,
        )
        .with_metadata("tenant_id", serde_json::json!(self.tenant_id().to_string()))
    }
}

/// Alias kept for ergonomic envelope construction in tests.
pub type GrantId = ImpersonationGrantId;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_carry_tenant_metadata() {
        let grant_id = Uuid::now_v7();
        let tenant_id = Uuid::now_v7();
        let event = PlatformAdminEvent::ImpersonationStarted {
            grant_id,
            actor_id: Uuid::now_v7(),
            tenant_id,
            target_user_id: Uuid::now_v7(),
            mode: ImpersonationMode::ReadOnly,
            reason: "support ticket".to_string(),
            expires_at: chrono::Utc::now(),
        };
        let envelope = event.envelope();
        assert_eq!(envelope.aggregate, "platform_admin.impersonation");
        assert_eq!(envelope.aggregate_id, grant_id);
        assert_eq!(envelope.metadata["tenant_id"], tenant_id.to_string());
        assert_eq!(envelope.event_type, "platform_admin.impersonation_started");
    }
}
