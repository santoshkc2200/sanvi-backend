//! Serialized, retryable authorization snapshots. Postgres owns both the
//! cross-worker lock and the last successfully applied snapshot. Keto calls
//! are idempotent; a crash before commit repeats the complete reconciliation.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use sanvi_identity::domain::MembershipRepository;
use sanvi_kernel::{RoleId, TenantId, UserId};
use sanvi_persistence::{PlatformScope, TxManager};

use crate::{
    application::storage,
    domain::{
        AccessError, AccessProjection, PERMISSIONS, PermissionStore, RoleRepository, RoleScope,
        Tuple,
    },
};

pub struct SqlxAccessProjection {
    tx: TxManager,
    roles: Arc<dyn RoleRepository>,
    memberships: Arc<dyn MembershipRepository>,
    store: Arc<dyn PermissionStore>,
}

impl SqlxAccessProjection {
    pub fn new(
        tx: TxManager,
        roles: Arc<dyn RoleRepository>,
        memberships: Arc<dyn MembershipRepository>,
        store: Arc<dyn PermissionStore>,
    ) -> Self {
        Self {
            tx,
            roles,
            memberships,
            store,
        }
    }

    async fn locked(&self, key: String) -> Result<PlatformScope<'_>, AccessError> {
        let mut scope = self
            .tx
            .begin_unscoped("authorization_projection", "access-worker")
            .await
            .map_err(storage)?;
        // Transaction locks release on error, cancellation or connection loss.
        // All workers use the same namespaced aggregate key. Hash collisions
        // only serialize unrelated aggregates; they cannot weaken exclusion.
        sqlx::query!(
            "SELECT 1 AS locked FROM pg_advisory_xact_lock(hashtextextended($1, 0))",
            key
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(storage)?;
        Ok(scope)
    }
}

#[async_trait]
impl AccessProjection for SqlxAccessProjection {
    async fn reconcile_membership(
        &self,
        tenant: TenantId,
        user: UserId,
    ) -> Result<(), AccessError> {
        let mut scope = self
            .locked(format!("access:membership:{tenant}:{user}"))
            .await?;
        let previous = sqlx::query!("SELECT role_objects FROM access.membership_projection_checkpoints WHERE tenant_id = $1 AND user_id = $2", tenant.as_i64(), user.as_i64())
            .fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.map(|row| row.role_objects);
        let roles = self.roles.assignable_to(&mut scope, tenant).await?;
        // Read after obtaining the serialization lock. Event versions/deltas
        // never decide the target state, including when events arrive reversed.
        let membership = self
            .memberships
            .list_for_user_platform(&mut scope, user)
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?
            .into_iter()
            .find(|member| member.tenant_id() == tenant);
        let current_roles = membership
            .as_ref()
            .filter(|member| member.is_active())
            .map(|member| member.role_ids())
            .unwrap_or_default();
        let desired: BTreeSet<String> = roles
            .iter()
            .filter(|role| current_roles.contains(&role.id()))
            .map(|role| format!("{tenant}:{}", role.key()))
            .collect();
        // Include the catalog on first run to repair tuples written by older
        // releases before snapshot checkpoints existed. Never touch platform
        // role assignments belonging to the same person.
        let removable: BTreeSet<String> = previous
            .unwrap_or_default()
            .into_iter()
            .chain(roles.iter().map(|role| format!("{tenant}:{}", role.key())))
            .collect();
        let mut removals: Vec<Tuple> = removable
            .iter()
            .map(|object| Tuple::role_membership(object, user))
            .collect();
        removals.push(Tuple::tenant_membership(tenant, user));
        self.store.delete(&removals).await?;
        let mut additions: Vec<Tuple> = desired
            .iter()
            .map(|object| Tuple::role_membership(object, user))
            .collect();
        if membership.as_ref().is_some_and(|member| member.is_active()) {
            additions.push(Tuple::tenant_membership(tenant, user));
        }
        if !additions.is_empty() {
            self.store.write(&additions).await?;
        }
        let objects: Vec<String> = desired.into_iter().collect();
        sqlx::query!("INSERT INTO access.membership_projection_checkpoints (tenant_id, user_id, role_objects) VALUES ($1, $2, $3) ON CONFLICT (tenant_id, user_id) DO UPDATE SET role_objects = EXCLUDED.role_objects, applied_at = now()", tenant.as_i64(), user.as_i64(), &objects)
            .execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        scope.commit().await.map_err(storage)
    }

    async fn reconcile_role(
        &self,
        role_id: RoleId,
        previous_object: &str,
    ) -> Result<(), AccessError> {
        let mut scope = self.locked(format!("access:role:{role_id}")).await?;
        let previous = sqlx::query!("SELECT role_objects, permissions FROM access.role_projection_checkpoints WHERE role_id = $1", role_id.as_i64())
            .fetch_optional(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?.map(|row| (row.role_objects, row.permissions));
        let role = self.roles.find_by_id(&mut scope, role_id).await?;
        let mut objects = BTreeSet::new();
        let mut permissions = BTreeSet::new();
        if let Some(role) = &role {
            permissions.extend(role.permissions().iter().cloned());
            if role.scope() == RoleScope::Tenant && role.tenant_id().is_none() {
                let members = self
                    .memberships
                    .list_all_platform(&mut scope)
                    .await
                    .map_err(|err| AccessError::Storage(err.to_string()))?;
                objects.extend(
                    members
                        .iter()
                        .map(|member| format!("{}:{}", member.tenant_id(), role.key())),
                );
            } else {
                objects.insert(role.tuple_object());
            }
        }
        let (previous_objects, previous_permissions) = previous.unwrap_or_default();
        let obsolete_objects: BTreeSet<String> = previous_objects
            .into_iter()
            .chain(objects.iter().cloned())
            .chain(std::iter::once(previous_object.to_owned()))
            .collect();
        // Registry cleanup also repairs pre-checkpoint deployments and partial
        // writes. Checkpoint permissions retain names removed from the registry.
        let removable_permissions: BTreeSet<String> = previous_permissions
            .into_iter()
            .chain(
                PERMISSIONS
                    .iter()
                    .map(|permission| permission.key.to_owned()),
            )
            .chain(permissions.iter().cloned())
            .collect();
        let removals: Vec<Tuple> = obsolete_objects
            .iter()
            .flat_map(|object| {
                removable_permissions
                    .iter()
                    .map(move |permission| Tuple::permission_grant(permission, object))
            })
            .collect();
        self.store.delete(&removals).await?;
        let additions: Vec<Tuple> = objects
            .iter()
            .flat_map(|object| {
                permissions
                    .iter()
                    .map(move |permission| Tuple::permission_grant(permission, object))
            })
            .collect();
        if !additions.is_empty() {
            self.store.write(&additions).await?;
        }
        let objects: Vec<String> = objects.into_iter().collect();
        let permissions: Vec<String> = permissions.into_iter().collect();
        sqlx::query!("INSERT INTO access.role_projection_checkpoints (role_id, role_objects, permissions) VALUES ($1, $2, $3) ON CONFLICT (role_id) DO UPDATE SET role_objects = EXCLUDED.role_objects, permissions = EXCLUDED.permissions, applied_at = now()", role_id.as_i64(), &objects, &permissions)
            .execute(&mut **scope.tx_mut().as_inner()).await.map_err(storage)?;
        scope.commit().await.map_err(storage)
    }
}
