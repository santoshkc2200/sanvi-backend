//! Ports (traits) implemented by driven adapters.
//!
//! Scoping follows the tenancy context's structural guard: tenant-scoped
//! ports take `&mut ScopedTx` (RLS-filtered by construction), platform-wide
//! ports take `&mut PlatformScope` (audited). Ports that reach *outside*
//! the process (Kratos, notifications) take no scope.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{EventEnvelope, RoleId, TenantId};
use sanvi_persistence::{PlatformScope, ScopedTx};
use uuid::Uuid;

use crate::domain::IdentityError;
use crate::domain::events::IdentityEvent;
use crate::domain::invitation::{Invitation, InvitationId};
use crate::domain::link_intent::LinkIntent;
use crate::domain::membership::Membership;
use crate::domain::user::{LinkedAccount, User, UserId};
use crate::domain::values::{Email, ProviderId, ProviderSubject};

/// Platform-scoped storage for the user projection (`identity.users` has no
/// tenant_id column — users exist across tenants).
#[async_trait]
pub trait UserRepository: Send + Sync {
    async fn save(&self, scope: &mut PlatformScope<'_>, user: &User) -> Result<(), IdentityError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: UserId,
    ) -> Result<Option<User>, IdentityError>;

    async fn find_by_kratos(
        &self,
        scope: &mut PlatformScope<'_>,
        kratos_identity_id: Uuid,
    ) -> Result<Option<User>, IdentityError>;

    /// Active users only (the partial unique index guarantees one).
    async fn find_by_email(
        &self,
        scope: &mut PlatformScope<'_>,
        email: &Email,
    ) -> Result<Option<User>, IdentityError>;
}

/// Tenant-scoped memberships (`identity.memberships` + role rows).
#[async_trait]
pub trait MembershipRepository: Send + Sync {
    /// Insert (or fully replace) a membership, including its roles.
    async fn save(
        &self,
        scope: &mut ScopedTx<'_>,
        membership: &Membership,
    ) -> Result<(), IdentityError>;

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<Option<Membership>, IdentityError>;

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Membership>, IdentityError>;

    /// Membership rows joined with the member's email (the read model for
    /// the member list; the join reaches identity.users, which carries no
    /// tenant scoping).
    async fn list_with_emails(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<(Membership, Email)>, IdentityError>;

    async fn remove(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<(), IdentityError>;

    /// Phase-05 erasure: delete every membership (and membership role) of
    /// one user across all tenants, under the audited platform scope.
    async fn remove_all_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, IdentityError>;

    /// Cross-context read for the access context's Keto sync: every active
    /// membership with its roles, read under the audited platform scope.
    async fn list_all_platform(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<Membership>, IdentityError>;

    /// Active member count for a tenant (the platform admin read model;
    /// read under the audited platform scope).
    async fn count_members_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<u32, IdentityError>;

    /// True when `user_id` holds `role_key` in `tenant_id` (last-owner guard).
    async fn count_role_holders(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        role_id: RoleId,
    ) -> Result<u32, IdentityError>;

    /// Every active membership `user_id` holds, across every tenant (the
    /// `GET /api/v1/me` read model — a staff member's own tenant list is not
    /// something a `ScopedTx` can answer, since it doesn't yet know which
    /// tenant to scope to). Read under the audited platform scope, same as
    /// [`Self::list_all_platform`].
    async fn list_for_user_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<Vec<Membership>, IdentityError>;
}

/// Global `(provider, subject)` → user binding.
#[async_trait]
pub trait LinkedAccountRepository: Send + Sync {
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        provider: &ProviderId,
        subject: &ProviderSubject,
    ) -> Result<Option<LinkedAccount>, IdentityError>;

    async fn link(
        &self,
        scope: &mut PlatformScope<'_>,
        account: &LinkedAccount,
    ) -> Result<(), IdentityError>;

    /// Phase-05 erasure: unlink every provider account of one user.
    async fn remove_all(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, IdentityError>;

    async fn list_for_user(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<Vec<LinkedAccount>, IdentityError>;
}

/// Tenant-scoped invitations.
#[async_trait]
pub trait InvitationRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut ScopedTx<'_>,
        invitation: &Invitation,
    ) -> Result<(), IdentityError>;

    async fn find_by_token_hash(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        token_hash: &str,
    ) -> Result<Option<Invitation>, IdentityError>;

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: InvitationId,
    ) -> Result<Option<Invitation>, IdentityError>;

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Invitation>, IdentityError>;

    /// Phase-05 erasure: delete invitations carrying this email address
    /// across all tenants, under the audited platform scope.
    async fn remove_by_email_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        email: &str,
    ) -> Result<usize, IdentityError>;

    async fn remove(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: InvitationId,
    ) -> Result<(), IdentityError>;
}

/// Single-use linking intents (platform-scoped: created pre-authentication).
#[async_trait]
pub trait LinkIntentRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        intent: &LinkIntent,
    ) -> Result<(), IdentityError>;

    /// Peek without consuming (validation happens before the single-use
    /// delete, so a wrong-email attempt cannot burn the nonce).
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        nonce_hash: &str,
    ) -> Result<Option<LinkIntent>, IdentityError>;

    /// Atomically consume the intent: returns it exactly once per nonce.
    async fn take(
        &self,
        scope: &mut PlatformScope<'_>,
        nonce_hash: &str,
    ) -> Result<Option<LinkIntent>, IdentityError>;

    /// Phase-05 erasure: delete link intents whose payload carries this
    /// email address.
    async fn remove_by_email_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        email: &str,
    ) -> Result<usize, IdentityError>;
}

/// The Kratos REST surface this context uses (driven adapter).
#[async_trait]
pub trait KratosClient: Send + Sync {
    /// `GET /sessions/whoami` for a session token. `Ok(None)` = no such
    /// session (401); `Err` = backend trouble (fail closed).
    async fn whoami(&self, session_token: &str) -> Result<Option<SessionInfo>, IdentityError>;

    /// `DELETE /admin/sessions/{id}` (the public API refuses to revoke the
    /// session a request is authenticated with; self-revocation goes
    /// through the admin endpoint, guarded upstream by the whoami check).
    async fn revoke_session(&self, session_id: &str) -> Result<(), IdentityError>;

    /// Best-effort `PATCH /admin/identities/{id}` to record our user id in
    /// `metadata_public.user_id` (diagnostics; the projection row is the
    /// source of truth).
    async fn set_metadata_user_id(
        &self,
        kratos_identity_id: Uuid,
        user_id: UserId,
    ) -> Result<(), IdentityError>;

    /// Look up a Kratos identity by id (used by reconciliation tooling).
    async fn get_identity(
        &self,
        kratos_identity_id: Uuid,
    ) -> Result<Option<SessionInfo>, IdentityError>;

    /// DELETE /admin/identities/{id} — the phase-05 erasure tombstone. A
    /// missing identity (or no admin URL) is success: the identity is gone
    /// either way.
    async fn delete_identity(&self, kratos_identity_id: Uuid) -> Result<(), IdentityError>;
}

/// What the session verifier needs to know about a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub identity_id: Uuid,
    pub email: Email,
    pub email_verified: bool,
    pub locale: Option<crate::domain::values::Locale>,
    pub aal: String,
    pub methods: Vec<String>,
    pub authenticated_at: Option<DateTime<Utc>>,
    /// `provider:subject` identifiers from the identity's OIDC credentials,
    /// if any — the auto-linking input.
    pub oidc_identifiers: Vec<(ProviderId, ProviderSubject)>,
}

/// Short-TTL cache of verified sessions, keyed by session-token hash.
#[async_trait]
pub trait SessionCache: Send + Sync {
    async fn get(&self, token_hash: &str) -> Result<Option<SessionInfo>, IdentityError>;
    async fn put(
        &self,
        token_hash: &str,
        session: &SessionInfo,
        ttl_secs: u64,
    ) -> Result<(), IdentityError>;
    /// Invalidate every cached entry for a session id (logout webhook).
    async fn invalidate_session(&self, session_id: &str) -> Result<(), IdentityError>;
}

/// Outbound mail: invitations and security notices. The notifications
/// context owns real delivery (later phase); this port keeps identity
/// decoupled from it.
#[async_trait]
pub trait Notifications: Send + Sync {
    async fn send_invitation(
        &self,
        tenant_id: TenantId,
        email: &Email,
        tenant_name: &str,
        invite_url: &str,
    ) -> Result<(), IdentityError>;

    /// Security notice: a new provider was linked, a session was revoked, …
    async fn send_security_notice(
        &self,
        email: &Email,
        kind: &str,
        detail: &str,
    ) -> Result<(), IdentityError>;
}

/// In-process view of the access context's role catalog: validates that the
/// role ids an invitation/membership references exist and are assignable in
/// the target tenant (implemented by the access context's adapter).
#[async_trait]
pub trait RoleCatalog: Send + Sync {
    /// All assignable roles for a tenant: its custom roles plus the system
    /// templates. Returns the role key for each id (needed for guards).
    async fn assignable_roles(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(RoleId, String)>, IdentityError>;

    /// The role key for a tenant-scoped role id, if assignable there.
    async fn role_key(
        &self,
        tenant_id: TenantId,
        role_id: RoleId,
    ) -> Result<Option<String>, IdentityError>;

    /// The union of every permission the given roles grant — the `GET
    /// /api/v1/me` read model's source for "what can this member do in this
    /// tenant" (permission *keys* only; the backend still enforces every
    /// mutation with its own `require_permission!` guard regardless of what
    /// the frontend renders from this list).
    async fn permissions_for_roles(
        &self,
        tenant_id: TenantId,
        role_ids: &[RoleId],
    ) -> Result<Vec<String>, IdentityError>;
}

/// In-process view of the tenancy context's tenant catalog: resolves the
/// display metadata (`GET /api/v1/me`'s membership list needs a name/slug
/// per tenant, not just the id) without identity depending on the tenancy
/// crate directly — implemented by an adapter built at the composition root,
/// same shape as [`RoleCatalog`].
#[async_trait]
pub trait TenantCatalog: Send + Sync {
    /// Resolve every tenant summary for the given ids in one call. Callers
    /// with multiple ids (e.g. `GET /api/v1/me`'s per-membership loop) must
    /// batch through this rather than looping single lookups, since each
    /// call opens the audited unscoped-transaction escape hatch exactly
    /// once regardless of `tenant_ids.len()`.
    async fn summaries(&self, tenant_ids: &[TenantId])
    -> Result<Vec<TenantSummary>, IdentityError>;
}

/// The subset of a tenant's identity a membership listing needs to render.
#[derive(Debug, Clone)]
pub struct TenantSummary {
    pub tenant_id: TenantId,
    pub slug: String,
    pub name: String,
}

/// Domain events → transactional outbox.
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[IdentityEvent],
    ) -> Result<(), IdentityError>;

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        events: &[IdentityEvent],
    ) -> Result<(), IdentityError>;

    async fn publish_envelopes_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        envelopes: &[EventEnvelope],
    ) -> Result<(), IdentityError>;
}

/// Source of new user/invitation ids (tests substitute deterministic ones).
pub trait IdGenerator: Send + Sync {
    fn next_user_id(&self) -> Result<UserId, IdentityError>;
    fn next_invitation_id(&self) -> Result<InvitationId, IdentityError>;
}

/// The system clock (tests substitute fixed times).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Cross-context membership read model for the access context's tuple sync:
/// implemented by an identity adapter, consumed by access.
#[async_trait]
pub trait MembershipSource: Send + Sync {
    /// Every active membership across all tenants (audited platform read).
    async fn all_memberships(&self) -> Result<Vec<Membership>, IdentityError>;
}
