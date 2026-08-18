//! The in-process `RoleCatalog` adapter the identity context consumes:
//! which roles exist and are assignable in a tenant.

use async_trait::async_trait;
use sanvi_identity::domain::{IdentityError, RoleCatalog};
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
        tenant_id: uuid::Uuid,
    ) -> Result<Vec<(uuid::Uuid, String)>, IdentityError> {
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
            .map(|role| (role.id().as_uuid(), role.key().to_string()))
            .collect())
    }

    async fn role_key(
        &self,
        tenant_id: uuid::Uuid,
        role_id: uuid::Uuid,
    ) -> Result<Option<String>, IdentityError> {
        let roles = self.assignable_roles(tenant_id).await?;
        Ok(roles
            .into_iter()
            .find(|(id, _)| *id == role_id)
            .map(|(_, key)| key))
    }

    async fn permissions_for_roles(
        &self,
        _tenant_id: uuid::Uuid,
        role_ids: &[uuid::Uuid],
    ) -> Result<Vec<String>, IdentityError> {
        use crate::domain::roles::RoleId;

        let mut permissions = std::collections::BTreeSet::new();
        for role_id in role_ids {
            let mut scope = self
                .tx_manager
                .begin_unscoped("role_catalog", "identity")
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
            let role = self
                .roles
                .find_by_id(&mut scope, RoleId::from_uuid(*role_id))
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
            scope
                .commit()
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
            // A membership referencing a since-deleted role contributes no
            // permissions rather than failing the whole `/me` response.
            if let Some(role) = role {
                permissions.extend(role.permissions().iter().cloned());
            }
        }
        Ok(permissions.into_iter().collect())
    }
}
