//! The in-process `RoleCatalog` adapter the identity context consumes:
//! which roles exist and are assignable in a tenant.

use async_trait::async_trait;
use sanvi_identity::domain::{IdentityError, RoleCatalog};
use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::TxManager;

use crate::domain::RoleRepository;

pub struct SqlxRoleCatalog {
    tx_manager: TxManager,
    roles: std::sync::Arc<dyn RoleRepository>,
}

impl SqlxRoleCatalog {
    pub fn new(tx_manager: TxManager, roles: std::sync::Arc<dyn RoleRepository>) -> Self {
        Self { tx_manager, roles }
    }
}

#[async_trait]
impl RoleCatalog for SqlxRoleCatalog {
    async fn assignable_roles(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(RoleId, String)>, IdentityError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("role_catalog", "identity")
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        let roles = self
            .roles
            .assignable_to(&mut scope, tenant_id)
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(roles
            .into_iter()
            .map(|role| (role.id(), role.key().to_string()))
            .collect())
    }

    async fn role_key(
        &self,
        tenant_id: TenantId,
        role_id: RoleId,
    ) -> Result<Option<String>, IdentityError> {
        let roles = self.assignable_roles(tenant_id).await?;
        Ok(roles
            .into_iter()
            .find(|(id, _)| *id == role_id)
            .map(|(_, key)| key))
    }

    async fn permissions_for_roles(
        &self,
        tenant_id: TenantId,
        role_ids: &[RoleId],
    ) -> Result<Vec<String>, IdentityError> {
        // Resolve through `assignable_to`, the same tenant-scoped role set
        // `GrantMembership`/`UpdateMemberRoles` validate `role_ids` against
        // at grant time — not `find_by_id`, which has no notion of tenant
        // ownership and would silently resolve a role belonging to a
        // different tenant. This also opens the audited unscoped-transaction
        // escape hatch once per call instead of once per role id.
        let mut scope = self
            .tx_manager
            .begin_unscoped("role_catalog", "identity")
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        let assignable = self
            .roles
            .assignable_to(&mut scope, tenant_id)
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        scope
            .commit()
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let wanted: std::collections::HashSet<_> = role_ids.iter().copied().collect();
        let mut permissions = std::collections::BTreeSet::new();
        // A membership referencing a since-deleted role, or (should it ever
        // happen) a role belonging to a different tenant, contributes no
        // permissions rather than failing the whole `/me` response.
        for role in assignable {
            if wanted.contains(&role.id()) {
                permissions.extend(role.permissions().iter().cloned());
            }
        }
        Ok(permissions.into_iter().collect())
    }
}
