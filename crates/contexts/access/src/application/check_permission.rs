//! The authorization check every context routes through: registry lookup →
//! candidate roles → Keto membership, cached for a short TTL.
//!
//! Fail closed: an unknown permission denies; a Keto outage returns an
//! error (the HTTP gate maps it to 503) — access is never granted "because
//! the backend was down".

use std::sync::Arc;

use sanvi_persistence::TxManager;
use uuid::Uuid;

use crate::application::storage;
use crate::domain::AccessError;
use crate::domain::permissions::{Permission, PermissionScope, lookup};
use crate::domain::ports::{
    Authorizer, Decision, DecisionCache, PermissionStore, ResourceRef, RoleRepository,
};
use crate::domain::roles::RoleScope;

pub struct CheckPermission {
    tx_manager: TxManager,
    roles: Arc<dyn RoleRepository>,
    store: Arc<dyn PermissionStore>,
}

impl CheckPermission {
    pub fn new(
        tx_manager: TxManager,
        roles: Arc<dyn RoleRepository>,
        store: Arc<dyn PermissionStore>,
    ) -> Self {
        Self {
            tx_manager,
            roles,
            store,
        }
    }

    /// The uncached core of the check.
    pub async fn check_raw(
        &self,
        user_id: Uuid,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Decision, AccessError> {
        let Some(def) = lookup(permission.as_str()) else {
            // Fail closed on undeclared permissions (also caught by the
            // catalog CI test).
            tracing::warn!(permission = %permission, "authorization check for undeclared permission");
            return Ok(Decision::Denied);
        };

        match def.scope {
            // Any authenticated subject may (the gate guarantees a subject).
            PermissionScope::Authenticated => return Ok(Decision::Allowed),
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
                return Ok(Decision::Denied);
            }
        };

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

        let objects: Vec<String> = candidates
            .iter()
            .map(|role| role_tuple_object(role, tenant_id))
            .collect();
        if objects.is_empty() {
            return Ok(Decision::Denied);
        }

        let results = self.store.check_membership(user_id, &objects).await?;
        Ok(if results.into_iter().any(|allowed| allowed) {
            Decision::Allowed
        } else {
            Decision::Denied
        })
    }
}

/// The Keto role object for a candidate: platform roles are global
/// (`platform:{key}`); system tenant templates materialize per tenant
/// (`{tenant_id}:{key}`) because membership is always tenant-scoped.
fn role_tuple_object(role: &crate::domain::Role, tenant_id: Option<Uuid>) -> String {
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

    fn cache_key(&self, user_id: Uuid, permission: &str, resource: &ResourceRef) -> String {
        let resource_key = match resource {
            ResourceRef::Tenant(id) => format!("tenant:{id}"),
            ResourceRef::User(id) => format!("user:{id}"),
            ResourceRef::Platform => "platform".to_string(),
        };
        format!("{user_id}:{permission}:{resource_key}")
    }
}

#[async_trait::async_trait]
impl Authorizer for CachedAuthorizer {
    async fn check(
        &self,
        user_id: Uuid,
        permission: &Permission,
        resource: &ResourceRef,
    ) -> Result<Decision, AccessError> {
        let key = self.cache_key(user_id, permission.as_str(), resource);
        if let Some(decision) = self.cache.get(&key).await {
            return Ok(decision);
        }
        let decision = self.inner.check_raw(user_id, permission, resource).await?;
        self.cache.put(&key, decision, self.ttl_secs).await;
        Ok(decision)
    }
}
