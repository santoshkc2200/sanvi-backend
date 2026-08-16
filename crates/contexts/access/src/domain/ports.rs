//! Ports (traits) implemented by driven adapters.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::PlatformScope;
use uuid::Uuid;

use crate::domain::error::AccessError;
use crate::domain::permissions::Permission;
use crate::domain::roles::{Role, RoleId, RoleScope};

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
        user_id: Uuid,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Decision, AccessError>;
}

/// A concrete resource reference (mirrors the platform's shape; the gate
/// adapter converts between the two).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceRef {
    Tenant(Uuid),
    User(Uuid),
    Platform,
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
        tenant_id: Option<Uuid>,
        key: &str,
    ) -> Result<Option<Role>, AccessError>;

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: RoleId,
    ) -> Result<Option<Role>, AccessError>;

    /// Roles granting `permission` that could apply to `tenant_id`: the
    /// platform roles (scope Platform), the system tenant templates, and the
    /// tenant's custom roles.
    async fn granting(
        &self,
        scope: &mut PlatformScope<'_>,
        permission: &str,
        tenant_id: Option<Uuid>,
    ) -> Result<Vec<Role>, AccessError>;

    /// Every role (seeding and the tuple rebuild).
    async fn list_all(&self, scope: &mut PlatformScope<'_>) -> Result<Vec<Role>, AccessError>;

    /// Roles assignable to a tenant: its custom roles plus the system
    /// templates (used by the identity context's `RoleCatalog` adapter).
    async fn assignable_to(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: Uuid,
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
        user_id: Uuid,
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
        subject_id: Uuid,
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
    pub fn role_membership(role_object: &str, user_id: Uuid) -> Self {
        Self::Subject {
            namespace: "role".to_string(),
            object: role_object.to_string(),
            relation: "member".to_string(),
            subject_id: user_id,
        }
    }

    /// Tenant containment: `tenant:{tenant_id}#member@user:{user_id}`.
    pub fn tenant_membership(tenant_id: Uuid, user_id: Uuid) -> Self {
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
    async fn invalidate_user(&self, user_id: Uuid);
}

/// Source of new role ids (tests substitute deterministic generators).
pub trait IdGenerator: Send + Sync {
    fn next_role_id(&self) -> RoleId;
}

/// The system clock (tests substitute fixed times).
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}
