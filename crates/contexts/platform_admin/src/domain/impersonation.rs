//! The impersonation grant aggregate: a support operator's time-boxed,
//! read-only-by-default, fully audited session as a tenant user.
//!
//! Guardrails (per the phase-03 plan): duration ≤ 30 minutes, a reason is
//! mandatory, revocation is immediate (checked per request), and every
//! action carries the acting operator's identity (`on_behalf_of`).

use chrono::{DateTime, Utc};
use sanvi_kernel::Id;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::PlatformAdminError;

pub enum ImpersonationGrantTag {}
pub type ImpersonationGrantId = Id<ImpersonationGrantTag>;

/// How much the impersonated session may do (mirrors the HTTP layer's
/// mode; the adapter converts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImpersonationMode {
    ReadOnly,
    ReadWrite,
}

/// Read-only by default (the plan's guardrail).
impl Default for ImpersonationMode {
    fn default() -> Self {
        Self::ReadOnly
    }
}

impl ImpersonationMode {
    pub fn parse(raw: &str) -> Result<Self, PlatformAdminError> {
        match raw {
            "read_only" => Ok(Self::ReadOnly),
            "read_write" => Ok(Self::ReadWrite),
            other => Err(PlatformAdminError::InvalidSearch(format!(
                "unknown impersonation mode: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpersonationGrant {
    id: ImpersonationGrantId,
    actor_id: Uuid,
    tenant_id: Uuid,
    target_user_id: Uuid,
    reason: String,
    mode: ImpersonationMode,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
    revoked_by: Option<Uuid>,
}

impl ImpersonationGrant {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ImpersonationGrantId,
        actor_id: Uuid,
        tenant_id: Uuid,
        target_user_id: Uuid,
        reason: String,
        mode: ImpersonationMode,
        duration_minutes: u32,
        max_minutes: u32,
        now: DateTime<Utc>,
    ) -> Result<Self, PlatformAdminError> {
        if reason.trim().is_empty() {
            return Err(PlatformAdminError::MissingReason);
        }
        if duration_minutes == 0 {
            return Err(PlatformAdminError::InvalidDuration);
        }
        if duration_minutes > max_minutes {
            return Err(PlatformAdminError::DurationTooLong { max_minutes });
        }
        Ok(Self {
            id,
            actor_id,
            tenant_id,
            target_user_id,
            reason,
            mode,
            created_at: now,
            expires_at: now + chrono::Duration::minutes(i64::from(duration_minutes)),
            revoked_at: None,
            revoked_by: None,
        })
    }

    /// Re-hydration constructor (no validation).
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: ImpersonationGrantId,
        actor_id: Uuid,
        tenant_id: Uuid,
        target_user_id: Uuid,
        reason: String,
        mode: ImpersonationMode,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
        revoked_at: Option<DateTime<Utc>>,
        revoked_by: Option<Uuid>,
    ) -> Self {
        Self {
            id,
            actor_id,
            tenant_id,
            target_user_id,
            reason,
            mode,
            created_at,
            expires_at,
            revoked_at,
            revoked_by,
        }
    }

    pub fn id(&self) -> ImpersonationGrantId {
        self.id
    }
    pub fn actor_id(&self) -> Uuid {
        self.actor_id
    }
    pub fn tenant_id(&self) -> Uuid {
        self.tenant_id
    }
    pub fn target_user_id(&self) -> Uuid {
        self.target_user_id
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }
    pub fn mode(&self) -> ImpersonationMode {
        self.mode
    }
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
    pub fn revoked_at(&self) -> Option<DateTime<Utc>> {
        self.revoked_at
    }
    pub fn revoked_by(&self) -> Option<Uuid> {
        self.revoked_by
    }

    /// The grant may be exchanged for a session: not revoked, not expired.
    pub fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }

    /// Revoke (idempotent: revoking a revoked grant is a no-op).
    pub fn revoke(&mut self, revoked_by: Uuid, now: DateTime<Utc>) {
        if self.revoked_at.is_none() {
            self.revoked_at = Some(now);
            self.revoked_by = Some(revoked_by);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(duration: u32) -> ImpersonationGrant {
        ImpersonationGrant::new(
            ImpersonationGrantId::new(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            "investigating support ticket #123".to_string(),
            ImpersonationMode::ReadOnly,
            duration,
            30,
            Utc::now(),
        )
        .unwrap()
    }

    #[test]
    fn grants_require_a_reason_and_a_positive_duration() {
        assert!(matches!(
            ImpersonationGrant::new(
                ImpersonationGrantId::new(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                "   ".to_string(),
                ImpersonationMode::ReadOnly,
                10,
                30,
                Utc::now(),
            ),
            Err(PlatformAdminError::MissingReason)
        ));
        assert!(matches!(
            ImpersonationGrant::new(
                ImpersonationGrantId::new(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                "reason".to_string(),
                ImpersonationMode::ReadOnly,
                0,
                30,
                Utc::now(),
            ),
            Err(PlatformAdminError::InvalidDuration)
        ));
    }

    #[test]
    fn duration_is_capped_at_the_configured_maximum() {
        assert!(matches!(
            ImpersonationGrant::new(
                ImpersonationGrantId::new(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                Uuid::now_v7(),
                "reason".to_string(),
                ImpersonationMode::ReadOnly,
                31,
                30,
                Utc::now(),
            ),
            Err(PlatformAdminError::DurationTooLong { max_minutes: 30 })
        ));
    }

    #[test]
    fn grants_expire_exactly_on_time() {
        let grant = grant(30);
        assert!(grant.is_active(grant.expires_at() - chrono::Duration::seconds(1)));
        assert!(!grant.is_active(grant.expires_at()));
        assert!(!grant.is_active(grant.expires_at() + chrono::Duration::seconds(1)));
    }

    #[test]
    fn revocation_is_immediate_and_idempotent() {
        let mut grant = grant(30);
        let revoker = Uuid::now_v7();
        grant.revoke(revoker, Utc::now());
        assert!(!grant.is_active(Utc::now()));
        assert_eq!(grant.revoked_by(), Some(revoker));
        let first = grant.revoked_at();
        grant.revoke(revoker, Utc::now());
        assert_eq!(grant.revoked_at(), first, "revoke must be idempotent");
    }
}
