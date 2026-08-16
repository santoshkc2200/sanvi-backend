//! The cross-context `MembershipSource` implementation (identity's rows,
//! read under the audited platform scope).

use async_trait::async_trait;
use sanvi_identity::domain::{IdentityError, MembershipRepository, MembershipSource};
use sanvi_persistence::TxManager;

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
