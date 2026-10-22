//! The authorization check every context routes through: registry lookup →
//! candidate roles → Keto membership, cached for a short TTL.
//!
//! Fail closed: an unknown permission denies; a Keto outage returns an
//! error (the HTTP gate maps it to 503) — access is never granted "because
//! the backend was down".

use std::sync::Arc;

use sanvi_kernel::{TenantId, UserId};
use sanvi_persistence::TxManager;

use crate::application::storage;
use crate::domain::AccessError;
use crate::domain::permissions::{Permission, PermissionScope, lookup};
use crate::domain::ports::{
    Authorizer, Decision, DecisionCache, PermissionStore, ResourceRef, RoleRepository,
    TenantRoleSource,
};
use crate::domain::roles::RoleScope;

pub struct CheckPermission {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
    store: Arc<dyn PermissionStore>,
    tenant_roles: Arc<dyn TenantRoleSource>,
}

impl CheckPermission {
    pub fn new(
        tx_manager: TxManager,
        roles: Arc<dyn RoleRepository>,
        store: Arc<dyn PermissionStore>,
        tenant_roles: Arc<dyn TenantRoleSource>,
    ) -> Self {
        Self {
            tx_manager,
            roles,
            store,
            tenant_roles,
        }
    }

    /// The uncached core of the check.
    #[tracing::instrument(name = "access.check_raw", skip_all)]
    pub async fn check_raw(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Decision, AccessError> {
        self.check_raw_verified(user_id, permission, resource)
            .await
            .map(|(decision, _)| decision)
    }

    async fn check_raw_verified(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<(Decision, bool), AccessError> {
        self.check_raw_proven(user_id, permission, resource)
            .await
            .map(|(decision, verified, _)| (decision, verified))
    }

    async fn check_raw_proven(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<(Decision, bool, Option<String>), AccessError> {
        let Some(def) = lookup(permission.as_str()) else {
            // Fail closed on undeclared permissions (also caught by the
            // catalog CI test).
            tracing::warn!(permission = %permission, "authorization check for undeclared permission");
            return Ok((Decision::Denied, false, None));
        };

        match def.scope {
            // Any authenticated subject may (the gate guarantees a subject).
            PermissionScope::Authenticated => return Ok((Decision::Allowed, false, None)),
            PermissionScope::Platform | PermissionScope::Tenant => {}
        }

        // A tenant-scoped permission needs the tenant; a platform-scoped
        // permission needs no resource at all.
        let tenant_id = match (def.scope, resource) {
            (PermissionScope::Tenant, ResourceRef::Tenant(tenant_id)) => Some(*tenant_id),
            (PermissionScope::Platform, ResourceRef::Platform) => None,
            _ => {
                // Misconfiguration (e.g. tenant permission on a user
                // resource): deny rather than widen.
                return Ok((Decision::Denied, false, None));
            }
        };

        let objects = self
            .eligible_objects(user_id, permission, tenant_id)
            .await?;
        let revision = tenant_id.map(|_| format!("{objects:?}"));
        if objects.is_empty() {
            return Ok((Decision::Denied, false, revision));
        }
        let results = self.store.check_membership(user_id, &objects).await?;
        Ok((
            if results.into_iter().any(|allowed| allowed) {
                Decision::Allowed
            } else {
                Decision::Denied
            },
            true,
            revision,
        ))
    }

    async fn eligible_objects(
        &self,
        user_id: UserId,
        permission: &Permission,
        tenant_id: Option<TenantId>,
    ) -> Result<Vec<String>, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("authorize", "access")
            .await
            .map_err(storage)?;
        let candidates = self
            .roles
            .granting(&mut scope, permission.as_str(), tenant_id)
            .await?;
        scope.commit().await.map_err(storage)?;

        let active_roles = match tenant_id {
            Some(tenant_id) => self.tenant_roles.active_roles(tenant_id, user_id).await?,
            None => Vec::new(),
        };
        let mut objects: Vec<String> = candidates
            .iter()
            // Platform assignments remain Keto-owned and apply globally.
            // Tenant grants must also exist in the authoritative membership.
            .filter(|role| role.scope() == RoleScope::Platform || active_roles.contains(&role.id()))
            .map(|role| role_tuple_object(role, tenant_id))
            .collect();
        objects.sort();
        objects.dedup();
        Ok(objects)
    }
}

/// The decision-cache key format, shared by [`CachedAuthorizer`] (which
/// fills it) and the [`crate::application::degraded_authorizer::
/// DegradedAuthorizer`] (which reads it during a Keto outage). One format,
/// or the grace-window reads would look up keys the normal path never
/// wrote.
pub(crate) fn decision_cache_key(
    user_id: UserId,
    permission: &str,
    resource: &ResourceRef,
) -> String {
    let resource_key = match resource {
        ResourceRef::Tenant(id) => format!("tenant:{id}"),
        ResourceRef::User(id) => format!("user:{id}"),
        ResourceRef::Platform => "platform".to_string(),
    };
    format!("{user_id}:{permission}:{resource_key}")
}

pub(crate) fn authority_cache_key(base: &str, revision: &str) -> String {
    format!("{base}:authority:{revision}")
}

/// The Keto role object for a candidate: platform roles are global
/// (`platform:{key}`); system tenant templates materialize per tenant
/// (`{tenant_id}:{key}`) because membership is always tenant-scoped.
fn role_tuple_object(role: &crate::domain::Role, tenant_id: Option<TenantId>) -> String {
    match role.scope() {
        RoleScope::Platform => format!("platform:{}", role.key()),
        RoleScope::Tenant => match (role.tenant_id(), tenant_id) {
            (Some(tenant_id), _) => format!("{tenant_id}:{}", role.key()),
            (None, Some(tenant_id)) => format!("{tenant_id}:{}", role.key()),
            (None, None) => format!("template:{}", role.key()),
        },
    }
}

/// The `Authorizer` implementation with the short-TTL decision cache.
pub struct CachedAuthorizer {
    inner: Arc<CheckPermission>,
    cache: Arc<dyn DecisionCache>,
    ttl_secs: u64,
}

impl CachedAuthorizer {
    pub fn new(inner: Arc<CheckPermission>, cache: Arc<dyn DecisionCache>, ttl_secs: u64) -> Self {
        Self {
            inner,
            cache,
            ttl_secs,
        }
    }

    fn cache_key(&self, user_id: UserId, permission: &str, resource: &ResourceRef) -> String {
        decision_cache_key(user_id, permission, resource)
    }
}

#[async_trait::async_trait]
impl Authorizer for CachedAuthorizer {
    async fn cache_revision(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Option<String>, AccessError> {
        let ResourceRef::Tenant(tenant) = resource else {
            return Ok(None);
        };
        if lookup(permission.as_str()).is_none_or(|def| def.scope != PermissionScope::Tenant) {
            return Ok(None);
        }
        let objects = self
            .inner
            .eligible_objects(user_id, permission, Some(*tenant))
            .await?;
        Ok(Some(format!("{objects:?}")))
    }

    async fn check_fresh(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<(Decision, bool), AccessError> {
        let key = self.cache_key(user_id, permission.as_str(), resource);
        let (decision, verified, revision) = self
            .inner
            .check_raw_proven(user_id, permission, resource)
            .await?;
        let key = revision
            .map(|revision| authority_cache_key(&key, &revision))
            .unwrap_or(key);
        self.cache.put(&key, decision, self.ttl_secs).await;
        Ok((decision, verified))
    }

    async fn check(
        &self,
        user_id: UserId,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Decision, AccessError> {
        // Tenant membership and role permissions can change before their
        // projection/invalidation event arrives. Never let a cached allow
        // bypass the current identity and role catalog.
        if matches!(resource, ResourceRef::Tenant(_)) {
            return self
                .check_fresh(user_id, permission, resource)
                .await
                .map(|(decision, _)| decision);
        }
        let key = self.cache_key(user_id, permission.as_str(), resource);
        if let Some(decision) = self.cache.get(&key).await {
            return Ok(decision);
        }
        self.check_fresh(user_id, permission, resource)
            .await
            .map(|(decision, _)| decision)
    }
}
