//! The theming context's ports: repositories, the entitlement gate, tenant
//! resolution for the public endpoint, storage, time and outbox publishing.
//!
//! Cross-context hooks (`ThemeFeatureGate`, `TenantContextResolver`) are
//! defined here; the composition root adapts them over the access/tenancy
//! contexts. The theming context never depends on those crates.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::{PgPool, PlatformScope, ScopedTx};

use crate::domain::error::ThemingError;
use crate::domain::events::ThemingEvent;
use crate::domain::tenant_theme::{TenantTheme, ThemeState};
use crate::domain::theme::{Theme, ThemeId, ThemeVersion, ThemeVersionId};

/// Platform theme registry (`theming.themes` + `theming.theme_versions`).
/// Registry reads are unscoped (they are platform reference data); writes
/// run inside the audited platform scope.
#[async_trait]
pub trait ThemeRepository: Send + Sync {
    async fn list(&self, pool: &PgPool) -> Result<Vec<Theme>, ThemingError>;

    async fn get_by_key(&self, pool: &PgPool, key: &str) -> Result<Option<Theme>, ThemingError>;

    async fn insert(
        &self,
        scope: &mut PlatformScope<'_>,
        theme: &Theme,
    ) -> Result<(), ThemingError>;

    async fn set_status(
        &self,
        scope: &mut PlatformScope<'_>,
        theme_id: ThemeId,
        status: &str,
    ) -> Result<(), ThemingError>;

    /// The versions of one theme, newest first.
    async fn list_versions(
        &self,
        pool: &PgPool,
        theme_id: ThemeId,
    ) -> Result<Vec<ThemeVersion>, ThemingError>;

    /// A specific version by theme key + semver (join).
    async fn get_version(
        &self,
        pool: &PgPool,
        theme_key: &str,
        semver: &str,
    ) -> Result<Option<ThemeVersion>, ThemingError>;

    /// The newest non-yanked published version of a theme.
    async fn get_latest_version(
        &self,
        pool: &PgPool,
        theme_key: &str,
    ) -> Result<Option<ThemeVersion>, ThemingError>;

    async fn insert_version(
        &self,
        scope: &mut PlatformScope<'_>,
        version: &ThemeVersion,
    ) -> Result<(), ThemingError>;

    /// Withdraw a version (kill switch). Immutable rows: only `yanked_at`
    /// may change.
    async fn yank_version(
        &self,
        scope: &mut PlatformScope<'_>,
        theme_id: ThemeId,
        semver: &str,
        at: DateTime<Utc>,
    ) -> Result<(), ThemingError>;
}

/// Tenant theme rows (`theming.tenant_themes`, RLS) and the publish
/// snapshots (`theming.tenant_theme_revisions`, RLS).
#[async_trait]
pub trait TenantThemeRepository: Send + Sync {
    async fn get(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        state: ThemeState,
    ) -> Result<Option<TenantTheme>, ThemingError>;

    async fn upsert(
        &self,
        scope: &mut ScopedTx<'_>,
        theme: &TenantTheme,
    ) -> Result<(), ThemingError>;

    /// Atomically copy the draft onto the live row, snapshot the new state
    /// and bump the revision. Returns the new revision.
    async fn publish(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        published_by: Option<UserId>,
        now: DateTime<Utc>,
    ) -> Result<i32, ThemingError>;

    /// Atomically restore the previous snapshot onto the live row as a new
    /// revision (append-only history, monotonic revision). Returns the new
    /// revision.
    async fn rollback(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        published_by: Option<UserId>,
        now: DateTime<Utc>,
    ) -> Result<i32, ThemingError>;
}

/// The cross-context hook for entitlement checks (premium themes, custom
/// CSS, custom templates). Implemented by the composition root over the
/// access context's feature gate.
#[async_trait]
pub trait ThemeFeatureGate: Send + Sync {
    async fn is_enabled(&self, tenant_id: TenantId, feature: &str) -> Result<bool, ThemingError>;
}

/// A minimal tenant reference for the public theme endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTenantRef {
    pub tenant_id: TenantId,
    pub slug: String,
}

/// The cross-context hook for host → tenant resolution. The public theme
/// endpoint is a public path (no tenant middleware), so it resolves the
/// host itself; an unresolved host returns the platform default theme.
#[async_trait]
pub trait TenantContextResolver: Send + Sync {
    async fn resolve(&self, host: &str) -> Result<Option<ResolvedTenantRef>, ThemingError>;
}

/// Storage for uploaded brand assets (content-addressed files).
#[async_trait]
pub trait BrandAssetStore: Send + Sync {
    async fn put(
        &self,
        tenant_id: TenantId,
        digest: &str,
        ext: &str,
        bytes: &[u8],
    ) -> Result<(), ThemingError>;

    async fn delete(&self, tenant_id: TenantId, digest: &str, ext: &str) -> Result<(), ThemingError>;
}

/// Storage for published theme artifacts (content-addressed zips).
#[async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), ThemingError>;

    /// Confirm that an artifact exists and matches its pinned sha256.
    async fn verify(&self, key: &str, digest: &str) -> Result<(), ThemingError>;
}

/// Source of time, injectable for tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Outbox-backed event publishing (mirrors the other contexts' pattern).
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        event: &ThemingEvent,
    ) -> Result<(), ThemingError>;

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        event: &ThemingEvent,
    ) -> Result<(), ThemingError>;
}

/// Source of new theme identifiers.
pub trait IdGenerator: Send + Sync {
    fn next_theme_id(&self) -> Result<ThemeId, ThemingError>;
    fn next_theme_version_id(&self) -> Result<ThemeVersionId, ThemingError>;
}
