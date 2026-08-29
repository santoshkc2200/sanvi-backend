//! Domain events emitted by the platform_admin context. Every
//! tenant-scoped event carries the tenant id in envelope metadata.

use sanvi_kernel::{EventEnvelope, EventId, TenantId, UserId};
use serde::Serialize;

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
        grant_id: ImpersonationGrantId,
        actor_id: UserId,
        tenant_id: TenantId,
        target_user_id: UserId,
        mode: ImpersonationMode,
        reason: String,
        expires_at: chrono::DateTime<chrono::Utc>,
    },
    ImpersonationEnded {
        grant_id: ImpersonationGrantId,
        actor_id: UserId,
        tenant_id: TenantId,
        target_user_id: UserId,
        /// The operator who revoked (revocations only; expiry is passive).
        ended_by: UserId,
    },
}

impl PlatformAdminEvent {
    pub fn name(&self) -> PlatformAdminEventName {
        match self {
            Self::ImpersonationStarted { .. } => PlatformAdminEventName::ImpersonationStarted,
            Self::ImpersonationEnded { .. } => PlatformAdminEventName::ImpersonationEnded,
        }
    }

    pub fn tenant_id(&self) -> TenantId {
        match self {
            Self::ImpersonationStarted { tenant_id, .. }
            | Self::ImpersonationEnded { tenant_id, .. } => *tenant_id,
        }
    }

    pub fn envelope(&self, id: EventId) -> EventEnvelope {
        let payload =
            serde_json::to_value(self).expect("platform admin events serialize infallibly");
        let aggregate_id = match self {
            Self::ImpersonationStarted { grant_id, .. }
            | Self::ImpersonationEnded { grant_id, .. } => grant_id.as_snowflake(),
        };
        EventEnvelope::new(
            id,
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
        let grant_id = ImpersonationGrantId::try_from(101_i64).unwrap();
        let tenant_id = TenantId::try_from(102_i64).unwrap();
        let event = PlatformAdminEvent::ImpersonationStarted {
            grant_id,
            actor_id: UserId::try_from(103_i64).unwrap(),
            tenant_id,
            target_user_id: UserId::try_from(104_i64).unwrap(),
            mode: ImpersonationMode::ReadOnly,
            reason: "support ticket".to_string(),
            expires_at: chrono::Utc::now(),
        };
        let envelope = event.envelope(EventId::try_from(105_i64).unwrap());
        assert_eq!(envelope.aggregate, "platform_admin.impersonation");
        assert_eq!(envelope.aggregate_id, grant_id.as_snowflake());
        assert_eq!(envelope.metadata["tenant_id"], tenant_id.to_string());
        assert_eq!(envelope.event_type, "platform_admin.impersonation_started");
    }
}
