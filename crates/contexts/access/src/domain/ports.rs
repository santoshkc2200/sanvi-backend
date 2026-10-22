//! Ports (traits) implemented by driven adapters.

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use sanvi_kernel::{RoleId, SnowflakeId, TenantId, UserId};
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::entitlements::{Entitlement, EntitlementSource, ResolvedEntitlement};
use crate::domain::error::AccessError;
use crate::domain::events::AccessEvent;
use crate::domain::features::{Feature, FeatureKey};
use crate::domain::permissions::Permission;
use crate::domain::roles::{Role, RoleScope};

/// The authorization decision every context consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    Denied,
}

/// The `Authorizer` port: the single way any context asks "may this subject
/// do that?" — implemented by the Keto-backed check with a short-TTL cache.
#[async_trait]
pub trait Authorizer: Send + Sync {
    async fn check(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Decision, AccessError>;

    /// Check the backend directly when a cached allow cannot establish
    /// availability (mutations and recovery from a known outage). The bool
    /// is true only when the decision actually contacted the backend.
    async fn check_fresh(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<(Decision, bool), AccessError> {
        self.check(user_id, permission, resource)
            .await
            .map(|decision| (decision, true))
    }

    /// Current authoritative grant revision for bounded tenant read grace.
    /// None means this authorizer cannot prove cached tenant grants remain
    /// current; outage fallback must deny them. Platform cache keys need no
    /// tenant membership revision.
    async fn cache_revision(
        &self,
        _user_id: UserId,
        _permission: &Permission,
        _resource: &ResourceRef,
    ) -> Result<Option<String>, AccessError> {
        Ok(None)
    }
}

/// A concrete resource reference (mirrors the platform's shape; the gate
/// adapter converts between the two).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRef {
    Tenant(TenantId),
    User(UserId),
    Platform,
}

/// Current active tenant roles from the identity system of record. A missing
/// or suspended membership has no roles, regardless of projection lag.
#[async_trait]
pub trait TenantRoleSource: Send + Sync {
    async fn active_roles(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<Vec<RoleId>, AccessError>;
}

/// Reconcile external authorization state from an authoritative snapshot.
/// Implementations serialize each aggregate across workers and only record
/// completed snapshots after external side effects succeed.
#[async_trait]
pub trait AccessProjection: Send + Sync {
    async fn reconcile_membership(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<(), AccessError>;
    async fn reconcile_role(
        &self,
        role_id: RoleId,
        previous_object: &str,
    ) -> Result<(), AccessError>;
}

/// The role catalog's storage. Platform and system-template reads happen
/// under the audited `PlatformScope`; tenant-scoped custom roles are RLS-
/// filtered and read under the same audited scope via the GUC bypass.
#[async_trait]
pub trait RoleRepository: Send + Sync {
    async fn save(&self, scope: &mut PlatformScope<'_>, role: &Role) -> Result<(), AccessError>;

    async fn find_by_key(
        &self,
        scope: &mut PlatformScope<'_>,
        scope_kind: RoleScope,
        tenant_id: Option<TenantId>,
        key: &str,
    ) -> Result<Option<Role>, AccessError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: RoleId,
    ) -> Result<Option<Role>, AccessError>;

    /// Delete a role (custom tenant roles only; system roles are rejected
    /// by the use case). Cascades to `role_permissions` and the identity
    /// context's `membership_roles`.
    async fn delete(&self, scope: &mut PlatformScope<'_>, id: RoleId) -> Result<(), AccessError>;

    /// Roles granting `permission` that could apply to `tenant_id`: the
    /// platform roles (scope Platform), the system tenant templates, and the
    /// tenant's custom roles.
    async fn granting(
        &self,
        scope: &mut PlatformScope<'_>,
        permission: &str,
        tenant_id: Option<TenantId>,
    ) -> Result<Vec<Role>, AccessError>;

    /// Every role (seeding and the tuple rebuild).
    async fn list_all(&self, scope: &mut PlatformScope<'_>) -> Result<Vec<Role>, AccessError>;

    /// Roles assignable to a tenant: its custom roles plus the system
    /// templates (used by the identity context's `RoleCatalog` adapter).
    async fn assignable_to(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Role>, AccessError>;
}

/// The Keto relation-tuple store (write: admin API, check: read API).
#[async_trait]
pub trait PermissionStore: Send + Sync {
    /// Insert tuples (idempotent per tuple).
    async fn write(&self, tuples: &[Tuple]) -> Result<(), AccessError>;

    /// Delete tuples.
    async fn delete(&self, tuples: &[Tuple]) -> Result<(), AccessError>;

    /// Delete every tuple in a namespace (full-reconciliation support).
    async fn clear_namespace(&self, namespace: &str) -> Result<(), AccessError>;

    /// Batch membership check: is `user_id` a member of any of `objects`?
    async fn check_membership(
        &self,
        user_id: UserId,
        objects: &[String],
    ) -> Result<Vec<bool>, AccessError>;
}

/// One Keto relation tuple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tuple {
    /// `object#relation@user:{user_id}`
    Subject {
        namespace: String,
        object: String,
        relation: String,
        subject_id: UserId,
    },
    /// `object#relation@subjectSet` (role → permission grants).
    SubjectSet {
        namespace: String,
        object: String,
        relation: String,
        subject_set: (String, String, String),
    },
}

impl Tuple {
    /// Role membership: `role:{object}#member@user:{user_id}`.
    pub fn role_membership(role_object: &str, user_id: UserId) -> Self {
        Self::Subject {
            namespace: "role".to_string(),
            object: role_object.to_string(),
            relation: "member".to_string(),
            subject_id: user_id,
        }
    }

    /// Tenant containment: `tenant:{tenant_id}#member@user:{user_id}`.
    pub fn tenant_membership(tenant_id: TenantId, user_id: UserId) -> Self {
        Self::Subject {
            namespace: "tenant".to_string(),
            object: tenant_id.to_string(),
            relation: "member".to_string(),
            subject_id: user_id,
        }
    }

    /// Role grants permission: `permission:{key}#granted@role:{object}#member`.
    pub fn permission_grant(permission: &str, role_object: &str) -> Self {
        Self::SubjectSet {
            namespace: "permission".to_string(),
            object: permission.to_string(),
            relation: "granted".to_string(),
            subject_set: (
                "role".to_string(),
                role_object.to_string(),
                "member".to_string(),
            ),
        }
    }
}

/// Short-TTL cache of permission decisions.
#[async_trait]
pub trait DecisionCache: Send + Sync {
    async fn get(&self, key: &str) -> Option<Decision>;
    async fn put(&self, key: &str, decision: Decision, ttl_secs: u64);
    async fn invalidate_user(&self, user_id: UserId);
    /// Drop everything (role authoring changed grants for arbitrary users).
    async fn invalidate_all(&self);
}

/// Source of new role ids (tests substitute deterministic generators).
pub trait IdGenerator: Send + Sync {
    fn next_role_id(&self) -> Result<RoleId, AccessError>;
}

/// The system clock (tests substitute fixed times).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The feature catalog's storage (global, not tenant-scoped).
#[async_trait]
pub trait FeatureRepository: Send + Sync {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        feature: &Feature,
    ) -> Result<(), AccessError>;

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        key: &FeatureKey,
    ) -> Result<Option<Feature>, AccessError>;

    async fn list_all(&self, scope: &mut PlatformScope<'_>) -> Result<Vec<Feature>, AccessError>;
}

/// Entitlement rows. Tenant-scoped reads run under `ScopedTx` (RLS-filtered
/// by construction); writes and cross-tenant maintenance run under the
/// audited `PlatformScope`.
#[async_trait]
pub trait EntitlementRepository: Send + Sync {
    /// The tenant's own rows (RLS sees only them).
    async fn list_for_tenant(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Entitlement>, AccessError>;

    /// The same rows, read under the audited platform scope.
    async fn list_for_tenant_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Entitlement>, AccessError>;

    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        entitlement: &Entitlement,
    ) -> Result<(), AccessError>;

    async fn delete(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        feature: &FeatureKey,
        source: EntitlementSource,
    ) -> Result<(), AccessError>;

    /// Phase-05: the operator override rows a user granted (granted_by).
    async fn list_by_granted_by(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<Vec<Entitlement>, AccessError>;

    /// Phase-05 erasure: clear `granted_by` references on override rows.
    async fn anonymise_granted_by(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, AccessError>;

    /// Non-expired override rows (the operator console's entitlement count).
    async fn count_active(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
        now: DateTime<Utc>,
    ) -> Result<u32, AccessError>;

    /// The same count for several tenants in one grouped statement, keyed by
    /// tenant — the platform tenant list uses it to avoid one query per row
    /// (TASK-021 11.2e). Tenants with none map to an absent entry (treat as
    /// zero).
    async fn count_active_batch(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_ids: &[TenantId],
        now: DateTime<Utc>,
    ) -> Result<std::collections::HashMap<TenantId, u32>, AccessError>;

    /// Expired override rows due for the sweep job.
    async fn expired(
        &self,
        scope: &mut PlatformScope<'_>,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Entitlement>, AccessError>;
}

/// Quota counters. Consumption is atomic in the database
/// (`INSERT ... ON CONFLICT DO UPDATE ... WHERE used + n <= limit
/// RETURNING`), so two concurrent requests cannot both take the last unit.
#[async_trait]
pub trait QuotaUsageStore: Send + Sync {
    /// Atomically consume `amount` against the period. Returns the new
    /// total, or `None` when the quota is exhausted.
    async fn consume(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        feature: &FeatureKey,
        period: NaiveDate,
        amount: i64,
        limit: i64,
    ) -> Result<Option<i64>, AccessError>;

    /// The current usage for a period.
    async fn usage(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        feature: &FeatureKey,
        period: NaiveDate,
    ) -> Result<i64, AccessError>;

    /// Delete periods that start before `before` (the rollover job);
    /// returns the number of rows removed.
    async fn rollover(
        &self,
        scope: &mut PlatformScope<'_>,
        before: NaiveDate,
    ) -> Result<u64, AccessError>;
}

/// The resolved entitlement set for one tenant, as served from the cache.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CachedEntitlements {
    pub entries: Vec<ResolvedEntitlement>,
    pub cached_at: DateTime<Utc>,
}

/// The entitlement cache (`sanvi:{tenant}:entitlements` in Redis, TTL 300 s
/// by default). Invalidation is event-driven: any entitlement or feature
/// event deletes the affected keys. Quota *consumption* never goes through
/// the cache — the atomic counter is always authoritative.
#[async_trait]
pub trait EntitlementCache: Send + Sync {
    async fn get(&self, tenant_id: TenantId) -> Option<CachedEntitlements>;
    async fn put(&self, tenant_id: TenantId, entries: &[ResolvedEntitlement], ttl_secs: u64);
    async fn invalidate(&self, tenant_id: TenantId);
    async fn invalidate_all(&self);
}

/// Publishes access events into the transactional outbox (same transaction
/// as the state change they describe).
#[async_trait]
pub trait EventPublisher: Send + Sync {
    async fn publish(
        &self,
        scope: &mut PlatformScope<'_>,
        events: &[AccessEvent],
    ) -> Result<(), AccessError>;

    async fn publish_scoped(
        &self,
        scope: &mut ScopedTx<'_>,
        events: &[AccessEvent],
    ) -> Result<(), AccessError>;
}

/// The outcome of the four-eyes check on a dangerous action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// The action may proceed immediately.
    NotRequired,
    /// The action was recorded as a pending approval; a second platform
    /// admin must approve it before it executes.
    Required { approval_id: SnowflakeId },
}

/// The four-eyes gate: an action marked `requires_approval` becomes a
/// pending request a second platform admin must approve. Implemented by the
/// `platform_admin` context; wired at the composition root.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn request_if_required(
        &self,
        scope: &mut PlatformScope<'_>,
        requested_by: UserId,
        action_type: &str,
        payload: serde_json::Value,
    ) -> Result<ApprovalOutcome, AccessError>;
}

/// The entitlement gate every context with optional capability consumes:
/// `is_enabled` for booleans, `remaining`/`consume` for quotas.
#[async_trait]
pub trait FeatureGate: Send + Sync {
    async fn is_enabled(
        &self,
        tenant_id: TenantId,
        feature: &FeatureKey,
    ) -> Result<bool, AccessError>;

    /// Units left in the current period (`None` for boolean features or
    /// unlimited quotas).
    async fn remaining(
        &self,
        tenant_id: TenantId,
        feature: &FeatureKey,
    ) -> Result<Option<i64>, AccessError>;

    /// Atomically consume `amount` units; fails with
    /// [`AccessError::QuotaExceeded`] when the period's limit is exhausted.
    async fn consume(
        &self,
        tenant_id: TenantId,
        feature: &FeatureKey,
        amount: i64,
    ) -> Result<(), AccessError>;
}
