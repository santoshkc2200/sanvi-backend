//! Ports (traits) implemented by driven adapters.
//!
//! The scoping guard is structural: tenant-scoped ports take
//! `&mut ScopedTx`, platform-wide ports take `&mut PlatformScope` — both are
//! capability tokens only constructible through `TxManager::begin_for` /
//! `begin_unscoped`, so a repository method that forgets its scope is a type
//! error, not a row leak.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::EventEnvelope;
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::tenant::{Tenant, TenantId, TenantStatus};
use crate::domain::values::TenantSlug;
use crate::domain::{TenancyError, events::TenantEvent};

/// Platform-scoped storage for the tenant aggregate (the `tenancy.tenants`
/// table has no tenant_id column — tenant rows *are* the tenant dimension).
#[async_trait]
pub trait TenantRepository: Send + Sync {
    /// Insert or update a tenant row (upsert on id; slug never changes).
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant: &Tenant,
    ) -> Result<(), TenancyError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: TenantId,
    ) -> Result<Option<Tenant>, TenancyError>;

    async fn find_by_slug(
        &self,
        scope: &mut PlatformScope<'_>,
        slug: &TenantSlug,
    ) -> Result<Option<Tenant>, TenancyError>;

    /// List tenants, cursor-paginated by id, optionally filtered by status.
    async fn list(
        &self,
        scope: &mut PlatformScope<'_>,
        after: Option<TenantId>,
        limit: u32,
        status: Option<TenantStatus>,
    ) -> Result<(Vec<Tenant>, Option<TenantId>), TenancyError>;

    /// Rich operator search (phase 03 platform admin console): text match
    /// on slug/display name, status filter, newest-first with a
    /// `(created_at, id)` cursor.
    async fn search(
        &self,
        scope: &mut PlatformScope<'_>,
        search: &TenantSearch,
    ) -> Result<(Vec<Tenant>, Option<TenantSearchCursor>), TenancyError>;

    /// Archived tenants whose purge has not been requested and whose
    /// retention window has passed (for the scheduled purge sweep).
    async fn due_for_purge_request(
        &self,
        scope: &mut PlatformScope<'_>,
        before: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Tenant>, TenancyError>;
}

/// Tenant-scoped key/value settings. Constructible only with a `ScopedTx`.
#[async_trait]
pub trait TenantSettingsRepository: Send + Sync {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        key: &str,
    ) -> Result<Option<serde_json::Value>, TenancyError>;

    async fn set(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), TenancyError>;

    async fn list_all(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<(String, serde_json::Value)>, TenancyError>;
}

/// Slug allocation: reservations arbitrate concurrent provisions before the
/// tenant row exists. The unique index is the final arbiter; the adapter
/// maps constraint violations to [`TenancyError::SlugTaken`].
#[async_trait]
pub trait SlugReservationPort: Send + Sync {
    /// Reserve a slug (idempotent per slug; conflicting slug → SlugTaken).
    async fn reserve(
        &self,
        scope: &mut PlatformScope<'_>,
        slug: &TenantSlug,
    ) -> Result<(), TenancyError>;

    /// Whether the slug is free for a new tenant: not reserved, not on the
    /// operator blocklist, no live tenant using it.
    async fn is_available(
        &self,
        scope: &mut PlatformScope<'_>,
        slug: &TenantSlug,
    ) -> Result<bool, TenancyError>;
}

/// Domain events → transactional outbox (implemented by the messaging
/// crate's writer; fakes in tests).
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[TenantEvent],
    ) -> Result<(), TenancyError>;

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        events: &[TenantEvent],
    ) -> Result<(), TenancyError>;

    /// Raw envelope access for cross-context machinery (e.g. the worker
    /// sweeps publishing purge requests).
    async fn publish_envelopes_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        envelopes: &[EventEnvelope],
    ) -> Result<(), TenancyError>;
}

/// Source of new tenant ids (tests substitute deterministic generators).
pub trait IdGenerator: Send + Sync {
    fn next_tenant_id(&self) -> TenantId;
}

/// The query shape for the operator tenant search (see
/// [`TenantRepository::search`]).
#[derive(Debug, Clone, Default)]
pub struct TenantSearch {
    /// Case-insensitive substring match on slug or display name.
    pub query: Option<String>,
    pub status: Option<TenantStatus>,
    /// Cursor of the last row of the previous page.
    pub after: Option<TenantSearchCursor>,
    pub limit: u32,
}

/// The search cursor: the `(created_at, id)` of the last returned row
/// (ordering is `created_at DESC, id DESC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantSearchCursor {
    pub created_at: DateTime<Utc>,
    pub id: TenantId,
}

/// The system clock (tests substitute fixed times).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Short-TTL cache of resolved tenants, keyed by slug and id.
///
/// Purely an optimisation: resolution must degrade to Postgres when the
/// cache backend is down — never fail open to "no tenant".
#[async_trait]
pub trait TenantResolutionCache: Send + Sync {
    async fn get_slug(&self, slug: &str) -> Result<Option<Tenant>, TenancyError>;
    async fn get_id(&self, id: uuid::Uuid) -> Result<Option<Tenant>, TenancyError>;
    async fn put(&self, tenant: &Tenant) -> Result<(), TenancyError>;
    async fn invalidate(&self, slug: &str, id: uuid::Uuid) -> Result<(), TenancyError>;
}
