//! Ports (traits) implemented by driven adapters.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::PlatformScope;
use uuid::Uuid;

use crate::domain::approvals::{ApprovalId, PendingApproval};
use crate::domain::error::PlatformAdminError;
use crate::domain::events::PlatformAdminEvent;
use crate::domain::impersonation::{ImpersonationGrant, ImpersonationGrantId};
use crate::domain::views::{TenantAdminView, TenantViewSearch};

/// Impersonation grant storage (platform-scoped).
#[async_trait]
pub trait ImpersonationGrantRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        grant: &ImpersonationGrant,
    ) -> Result<(), PlatformAdminError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ImpersonationGrantId,
    ) -> Result<Option<ImpersonationGrant>, PlatformAdminError>;

    /// Active (unrevoked, unexpired) grants, newest first.
    async fn list_active(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Option<Uuid>,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ImpersonationGrant>, PlatformAdminError>;

    /// Phase-05: grants a user created as the impersonating actor.
    async fn list_by_actor(
        &self,
        scope: &mut PlatformScope<'_>,
        actor_id: Uuid,
    ) -> Result<Vec<ImpersonationGrant>, PlatformAdminError>;

    /// Phase-05: grants into which a user was the impersonation target.
    async fn list_by_target(
        &self,
        scope: &mut PlatformScope<'_>,
        target_user_id: Uuid,
    ) -> Result<Vec<ImpersonationGrant>, PlatformAdminError>;

    /// Phase-05 erasure: revoke every grant a user authored (soft revoke —
    /// the table has no DELETE grant).
    async fn revoke_all_for_actor(
        &self,
        scope: &mut PlatformScope<'_>,
        actor_id: Uuid,
    ) -> Result<usize, PlatformAdminError>;

    /// Phase-05 erasure: redact `target_user_id` on every grant a user was
    /// impersonated under (the table has no DELETE grant, so this is a
    /// reference anonymisation, not a revoke).
    async fn anonymise_target(
        &self,
        scope: &mut PlatformScope<'_>,
        target_user_id: Uuid,
    ) -> Result<usize, PlatformAdminError>;

    /// How many active grants exist into a tenant (admin view summary).
    async fn count_active(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<u32, PlatformAdminError>;
}

/// Four-eyes pending approvals.
#[async_trait]
pub trait PendingApprovalRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        approval: &PendingApproval,
    ) -> Result<(), PlatformAdminError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: ApprovalId,
    ) -> Result<Option<PendingApproval>, PlatformAdminError>;

    /// Pending requests, oldest first (they age longest).
    async fn list_pending(
        &self,
        scope: &mut PlatformScope<'_>,
        limit: u32,
    ) -> Result<Vec<PendingApproval>, PlatformAdminError>;

    /// Phase-05: approvals requested by a user.
    async fn list_by_requester(
        &self,
        scope: &mut PlatformScope<'_>,
        requested_by: Uuid,
    ) -> Result<Vec<PendingApproval>, PlatformAdminError>;

    /// Phase-05 erasure: replace the requester/decider references with
    /// pseudonyms (rows themselves are compliance records).
    async fn anonymise_for_user(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: Uuid,
    ) -> Result<usize, PlatformAdminError>;
}

/// The operator tenant read model: tenancy basics + member/entitlement/
/// impersonation summaries, assembled over ports (no cross-schema SQL).
#[async_trait]
pub trait TenantAdminViewRepository: Send + Sync {
    async fn search(
        &self,
        scope: &mut PlatformScope<'_>,
        search: &TenantViewSearch,
    ) -> Result<
        (
            Vec<TenantAdminView>,
            Option<crate::domain::views::TenantViewCursor>,
        ),
        PlatformAdminError,
    >;

    async fn get(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
    ) -> Result<Option<TenantAdminView>, PlatformAdminError>;
}

/// The privacy-lock check (phase 05 wires the real jurisdiction-driven
/// implementation; until then it is a noop that never locks). Impersonation
/// into a locked tenant is refused.
#[async_trait]
pub trait PrivacyLock: Send + Sync {
    async fn is_locked(&self, tenant_id: Uuid) -> Result<bool, PlatformAdminError>;
}

/// Publishes platform-admin events into the transactional outbox.
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[PlatformAdminEvent],
    ) -> Result<(), PlatformAdminError>;
}

/// Source of new ids (tests substitute deterministic generators).
pub trait IdGenerator: Send + Sync {
    fn next_grant_id(&self) -> ImpersonationGrantId;
    fn next_approval_id(&self) -> ApprovalId;
}

/// The system clock (tests substitute fixed times).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The access context's entitlement administration, called by the
/// four-eyes executor and the operator endpoints. The composition root
/// wires the access context's `GrantEntitlement`/`RevokeEntitlement`/
/// `ListTenantOverrides`.
#[async_trait]
pub trait EntitlementAdmin: Send + Sync {
    /// Grant an override (may come back `ApprovalRequired` under the
    /// four-eyes policy).
    async fn grant_override(
        &self,
        actor_id: Uuid,
        tenant_id: Uuid,
        feature_key: String,
        enabled: bool,
        limit: Option<i64>,
        expires_at: Option<DateTime<Utc>>,
        reason: Option<String>,
    ) -> Result<GrantOverrideOutcome, PlatformAdminError>;

    /// The approval-execution path: re-run a grant with four-eyes skipped.
    async fn grant_override_approved(
        &self,
        requested_by: Uuid,
        tenant_id: Uuid,
        feature_key: String,
        enabled: bool,
        limit: Option<i64>,
        expires_at: Option<DateTime<Utc>>,
        reason: Option<String>,
    ) -> Result<(), PlatformAdminError>;

    async fn revoke_override(
        &self,
        actor_id: Uuid,
        tenant_id: Uuid,
        feature_key: String,
    ) -> Result<(), PlatformAdminError>;

    /// The tenant's stored override rows (operator console).
    async fn list_overrides(
        &self,
        tenant_id: Uuid,
    ) -> Result<Vec<sanvi_access::domain::Entitlement>, PlatformAdminError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantOverrideOutcome {
    Granted,
    ApprovalRequired { approval_id: Uuid },
}
