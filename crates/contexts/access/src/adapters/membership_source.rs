//! The cross-context `MembershipSource` implementation (identity's rows,
//! read under the audited platform scope).

use async_trait::async_trait;
use sanvi_identity::domain::{IdentityError, MembershipRepository, MembershipSource};
use sanvi_persistence::TxManager;

use crate::domain::{AccessError, TenantRoleSource};

pub struct SqlxTenantRoleSource {
    tx_manager: TxManager,
    memberships: std::sync::Arc<dyn MembershipRepository>,
}

impl SqlxTenantRoleSource {
    pub fn new(
        tx_manager: TxManager,
        memberships: std::sync::Arc<dyn MembershipRepository>,
    ) -> Self {
        Self {
            tx_manager,
            memberships,
        }
    }
}

#[async_trait]
impl TenantRoleSource for SqlxTenantRoleSource {
    async fn active_roles(
        &self,
        tenant_id: sanvi_kernel::TenantId,
        user_id: sanvi_kernel::UserId,
    ) -> Result<Vec<sanvi_kernel::RoleId>, AccessError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(crate::application::storage)?;
        let membership = self
            .memberships
            .find(&mut scope, tenant_id, user_id)
            .await
            .map_err(|err| AccessError::Storage(err.to_string()))?;
        scope.commit().await.map_err(crate::application::storage)?;
        Ok(membership
            .filter(|membership| membership.is_active())
            .map(|membership| membership.role_ids().to_vec())
            .unwrap_or_default())
    }
}

pub struct SqlxMembershipSource {
    tx_manager: TxManager,
    memberships: std::sync::Arc<dyn MembershipRepository>,
}

impl SqlxMembershipSource {
    pub fn new(
        tx_manager: TxManager,
        memberships: std::sync::Arc<dyn MembershipRepository>,
    ) -> Self {
        Self {
            tx_manager,
            memberships,
        }
    }
}

#[async_trait]
impl MembershipSource for SqlxMembershipSource {
    async fn all_memberships(
        &self,
    ) -> Result<Vec<sanvi_identity::domain::Membership>, IdentityError> {
        let mut scope = self
            .tx_manager
            .begin_unscoped("membership_source", "access")
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        let memberships = self.memberships.list_all_platform(&mut scope).await?;
        scope
            .commit()
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(memberships)
    }
}
