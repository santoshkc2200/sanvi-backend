//! Memberships: the user ↔ tenant ↔ roles binding.

use chrono::{DateTime, Utc};
use sanvi_kernel::Id;
use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::IdentityError;
use crate::domain::user::UserId;

pub enum MembershipTag {}
pub type MembershipId = Id<MembershipTag>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    Active,
    Suspended,
}

impl MembershipStatus {
    pub fn parse(raw: &str) -> Result<Self, IdentityError> {
        match raw {
            "active" => Ok(Self::Active),
            "suspended" => Ok(Self::Suspended),
            other => Err(IdentityError::invalid_membership_status(other)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
        }
    }
}

impl std::fmt::Display for MembershipStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A user's membership in one tenant, with the roles they hold there.
///
/// The `(tenant_id, user_id)` pair is the identity; roles are the
/// authorization surface and change independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Membership {
    tenant_id: uuid::Uuid,
    user_id: UserId,
    role_ids: Vec<uuid::Uuid>,
    status: MembershipStatus,
    invited_by: Option<UserId>,
    created_at: DateTime<Utc>,
}

impl Membership {
    pub fn grant(
        tenant_id: uuid::Uuid,
        user_id: UserId,
        role_ids: Vec<uuid::Uuid>,
        invited_by: Option<UserId>,
        now: DateTime<Utc>,
    ) -> Result<Self, IdentityError> {
        if role_ids.is_empty() {
            return Err(IdentityError::NoRoles);
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &role_ids {
            if !seen.insert(*id) {
                return Err(IdentityError::UnknownRole(*id));
            }
        }
        Ok(Self {
            tenant_id,
            user_id,
            role_ids,
            status: MembershipStatus::Active,
            invited_by,
            created_at: now,
        })
    }

    /// Rehydrate a stored row (adapter use only).
    pub fn from_parts(
        tenant_id: uuid::Uuid,
        user_id: UserId,
        role_ids: Vec<uuid::Uuid>,
        status: MembershipStatus,
        invited_by: Option<UserId>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            tenant_id,
            user_id,
            role_ids,
            status,
            invited_by,
            created_at,
        }
    }

    /// Replace the role set. Returns the delta for event publication and
    /// tuple sync.
    pub fn replace_roles(
        &mut self,
        role_ids: Vec<uuid::Uuid>,
    ) -> Result<(Vec<uuid::Uuid>, Vec<uuid::Uuid>), IdentityError> {
        if role_ids.is_empty() {
            return Err(IdentityError::NoRoles);
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &role_ids {
            if !seen.insert(*id) {
                return Err(IdentityError::UnknownRole(*id));
            }
        }
        let added: Vec<uuid::Uuid> = role_ids
            .iter()
            .filter(|id| !self.role_ids.contains(id))
            .copied()
            .collect();
        let removed: Vec<uuid::Uuid> = self
            .role_ids
            .iter()
            .filter(|id| !role_ids.contains(id))
            .copied()
            .collect();
        self.role_ids = role_ids;
        Ok((added, removed))
    }

    pub fn suspend(&mut self) -> Result<(), IdentityError> {
        if self.status != MembershipStatus::Active {
            return Err(IdentityError::InvalidMembershipStatus(
                "only active memberships can be suspended".to_string(),
            ));
        }
        self.status = MembershipStatus::Suspended;
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), IdentityError> {
        if self.status != MembershipStatus::Suspended {
            return Err(IdentityError::InvalidMembershipStatus(
                "only suspended memberships can be resumed".to_string(),
            ));
        }
        self.status = MembershipStatus::Active;
        Ok(())
    }

    pub fn tenant_id(&self) -> uuid::Uuid {
        self.tenant_id
    }
    pub fn user_id(&self) -> UserId {
        self.user_id
    }
    pub fn role_ids(&self) -> &[uuid::Uuid] {
        &self.role_ids
    }
    pub fn status(&self) -> MembershipStatus {
        self.status
    }
    pub fn invited_by(&self) -> Option<UserId> {
        self.invited_by
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role_ids() -> Vec<uuid::Uuid> {
        vec![uuid::Uuid::now_v7(), uuid::Uuid::now_v7()]
    }

    #[test]
    fn grant_requires_roles_and_rejects_duplicates() {
        let now = Utc::now();
        let mut roles = role_ids();
        assert!(Membership::grant(uuid::Uuid::now_v7(), UserId::new(), vec![], None, now).is_err());
        roles.push(roles[0]);
        assert!(Membership::grant(uuid::Uuid::now_v7(), UserId::new(), roles, None, now).is_err());
    }

    #[test]
    fn replace_roles_reports_deltas() {
        let now = Utc::now();
        let roles = role_ids();
        let mut membership = Membership::grant(
            uuid::Uuid::now_v7(),
            UserId::new(),
            roles.clone(),
            None,
            now,
        )
        .unwrap();
        let new_role = uuid::Uuid::now_v7();
        let (added, removed) = membership.replace_roles(vec![roles[0], new_role]).unwrap();
        assert_eq!(added, vec![new_role]);
        assert_eq!(removed, vec![roles[1]]);
        assert_eq!(membership.role_ids(), &[roles[0], new_role]);
    }

    #[test]
    fn suspend_resume_roundtrip() {
        let now = Utc::now();
        let mut membership =
            Membership::grant(uuid::Uuid::now_v7(), UserId::new(), role_ids(), None, now).unwrap();
        membership.suspend().unwrap();
        assert_eq!(membership.status(), MembershipStatus::Suspended);
        assert!(membership.suspend().is_err());
        membership.resume().unwrap();
        assert_eq!(membership.status(), MembershipStatus::Active);
    }
}
