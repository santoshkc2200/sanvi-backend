//! Domain events emitted by the identity context.
//!
//! Membership events carry the tenant id in envelope metadata (same
//! convention as tenancy) so the access context can sync Keto tuples
//! without parsing every payload.

use crate::domain::InvitationId;
use sanvi_kernel::{EventEnvelope, EventId, RoleId, TenantId};
use serde::Serialize;

use crate::domain::user::{LinkedVia, UserId};
use crate::domain::values::{Email, ProviderId};

/// Machine-readable event names, stable across versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityEventName {
    UserRegistered,
    UserEmailVerified,
    AccountLinked,
    AccountLinkRejected,
    SessionCreated,
    SessionRevoked,
    MembershipGranted,
    MembershipRolesChanged,
    MembershipRevoked,
    InvitationSent,
    InvitationAccepted,
    UserDeactivated,
}

impl IdentityEventName {
    pub const AGGREGATE: &'static str = "identity.user";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserRegistered => "identity.user_registered",
            Self::UserEmailVerified => "identity.user_email_verified",
            Self::AccountLinked => "identity.account_linked",
            Self::AccountLinkRejected => "identity.account_link_rejected",
            Self::SessionCreated => "identity.session_created",
            Self::SessionRevoked => "identity.session_revoked",
            Self::MembershipGranted => "identity.membership_granted",
            Self::MembershipRolesChanged => "identity.membership_roles_changed",
            Self::MembershipRevoked => "identity.membership_revoked",
            Self::InvitationSent => "identity.invitation_sent",
            Self::InvitationAccepted => "identity.invitation_accepted",
            Self::UserDeactivated => "identity.user_deactivated",
        }
    }
}

/// Events the identity context publishes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdentityEvent {
    UserRegistered {
        user_id: UserId,
        email: Email,
    },
    UserEmailVerified {
        user_id: UserId,
        email: Email,
    },
    AccountLinked {
        user_id: UserId,
        provider: ProviderId,
        subject: String,
        linked_via: LinkedVia,
    },
    AccountLinkRejected {
        user_id: Option<UserId>,
        provider: ProviderId,
        subject: String,
        email: Email,
    },
    SessionCreated {
        user_id: UserId,
        session_id: String,
    },
    SessionRevoked {
        user_id: Option<UserId>,
        session_id: String,
    },
    MembershipGranted {
        tenant_id: TenantId,
        user_id: UserId,
        role_ids: Vec<RoleId>,
    },
    MembershipRolesChanged {
        tenant_id: TenantId,
        user_id: UserId,
        role_ids: Vec<RoleId>,
    },
    MembershipRevoked {
        tenant_id: TenantId,
        user_id: UserId,
        role_ids: Vec<RoleId>,
    },
    InvitationSent {
        invitation_id: InvitationId,
        tenant_id: TenantId,
        email: Email,
    },
    InvitationAccepted {
        invitation_id: InvitationId,
        tenant_id: TenantId,
        user_id: UserId,
    },
    UserDeactivated {
        user_id: UserId,
    },
}

impl IdentityEvent {
    pub fn name(&self) -> IdentityEventName {
        match self {
            Self::UserRegistered { .. } => IdentityEventName::UserRegistered,
            Self::UserEmailVerified { .. } => IdentityEventName::UserEmailVerified,
            Self::AccountLinked { .. } => IdentityEventName::AccountLinked,
            Self::AccountLinkRejected { .. } => IdentityEventName::AccountLinkRejected,
            Self::SessionCreated { .. } => IdentityEventName::SessionCreated,
            Self::SessionRevoked { .. } => IdentityEventName::SessionRevoked,
            Self::MembershipGranted { .. } => IdentityEventName::MembershipGranted,
            Self::MembershipRolesChanged { .. } => IdentityEventName::MembershipRolesChanged,
            Self::MembershipRevoked { .. } => IdentityEventName::MembershipRevoked,
            Self::InvitationSent { .. } => IdentityEventName::InvitationSent,
            Self::InvitationAccepted { .. } => IdentityEventName::InvitationAccepted,
            Self::UserDeactivated { .. } => IdentityEventName::UserDeactivated,
        }
    }

    /// The tenant this event belongs to (membership/invitation events);
    /// `None` for user- and session-level events.
    pub fn tenant_id(&self) -> Option<TenantId> {
        match self {
            Self::MembershipGranted { tenant_id, .. }
            | Self::MembershipRolesChanged { tenant_id, .. }
            | Self::MembershipRevoked { tenant_id, .. }
            | Self::InvitationSent { tenant_id, .. }
            | Self::InvitationAccepted { tenant_id, .. } => Some(*tenant_id),
            _ => None,
        }
    }

    /// The user the event describes (where one exists).
    pub fn user_id(&self) -> Option<UserId> {
        match self {
            Self::UserRegistered { user_id, .. }
            | Self::UserEmailVerified { user_id, .. }
            | Self::AccountLinked { user_id, .. }
            | Self::SessionCreated { user_id, .. }
            | Self::MembershipGranted { user_id, .. }
            | Self::MembershipRolesChanged { user_id, .. }
            | Self::MembershipRevoked { user_id, .. }
            | Self::InvitationAccepted { user_id, .. }
            | Self::UserDeactivated { user_id } => Some(*user_id),
            Self::AccountLinkRejected { user_id, .. } => *user_id,
            Self::SessionRevoked { user_id, .. } => *user_id,
            Self::InvitationSent { .. } => None,
        }
    }

    /// The outbox envelope for this event.
    pub fn envelope(&self, id: EventId) -> EventEnvelope {
        let payload = serde_json::to_value(self).expect("identity events serialize infallibly");
        let aggregate_id =
            self.user_id()
                .map(|id| id.as_snowflake())
                .unwrap_or_else(|| match self {
                    Self::InvitationSent { invitation_id, .. }
                    | Self::InvitationAccepted { invitation_id, .. } => {
                        invitation_id.as_snowflake()
                    }
                    _ => sanvi_kernel::SnowflakeId::try_from(1_i64).expect("seed aggregate ID"),
                });
        let mut envelope = EventEnvelope::new(
            id,
            IdentityEventName::AGGREGATE,
            aggregate_id,
            self.name().as_str(),
            payload,
        );
        if let Some(tenant_id) = self.tenant_id() {
            envelope =
                envelope.with_metadata("tenant_id", serde_json::json!(tenant_id.to_string()));
        }
        envelope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_stable() {
        assert_eq!(
            IdentityEventName::MembershipGranted.as_str(),
            "identity.membership_granted"
        );
        assert_eq!(
            IdentityEventName::AccountLinked.as_str(),
            "identity.account_linked"
        );
    }

    #[test]
    fn membership_events_carry_tenant_metadata() {
        let tenant_id = TenantId::try_from(101_i64).unwrap();
        let user_id = UserId::try_from(102_i64).unwrap();
        let event = IdentityEvent::MembershipGranted {
            tenant_id,
            user_id,
            role_ids: vec![],
        };
        let envelope = event.envelope(EventId::try_from(103_i64).unwrap());
        assert_eq!(envelope.aggregate_id, user_id.as_snowflake());
        assert_eq!(envelope.metadata["tenant_id"], tenant_id.to_string());
        assert!(event.tenant_id().is_some());
    }

    #[test]
    fn user_events_have_no_tenant() {
        let event = IdentityEvent::UserRegistered {
            user_id: UserId::try_from(101_i64).unwrap(),
            email: Email::parse("a@b.co").unwrap(),
        };
        assert!(event.tenant_id().is_none());
    }
}
