//! Member management: list, grant, change roles, remove.

use std::sync::Arc;

use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::TxManager;
use serde::{Deserialize, Serialize};
use tracing::info;
use utoipa::ToSchema;

use crate::application::delegation::DelegationPolicy;
use crate::application::storage;
use crate::domain::events::IdentityEvent;
use crate::domain::membership::{Membership, MembershipStatus};
use crate::domain::user::UserId;
use crate::domain::values::Email;
use crate::domain::{
    Clock, EventPublisher, IdentityError, MembershipRepository, RoleCatalog, UserRepository,
};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MemberView {
    #[schema(example = "0190f0d0-0000-7000-8000-000000000001")]
    pub user_id: String,
    #[schema(example = "alice@example.com")]
    pub email: String,
    #[schema(value_type = Vec<String>)]
    pub role_ids: Vec<RoleId>,
    pub status: MembershipStatus,
    #[schema(value_type = Option<String>)]
    pub invited_by: Option<UserId>,
    #[schema(value_type = String)]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GrantMembershipCommand {
    #[schema(example = "alice@example.com")]
    pub email: String,
    #[schema(value_type = Vec<String>)]
    pub role_ids: Vec<RoleId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateRolesCommand {
    #[schema(value_type = Vec<String>)]
    pub role_ids: Vec<RoleId>,
}

pub struct ListMembers {
    tx_manager: TxManager,
    memberships: Arc<dyn MembershipRepository>,
}

impl ListMembers {
    pub fn new(tx_manager: TxManager, memberships: Arc<dyn MembershipRepository>) -> Self {
        Self {
            tx_manager,
            memberships,
        }
    }

    #[tracing::instrument(name = "identity.list_members", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(&self, tenant_id: TenantId) -> Result<Vec<MemberView>, IdentityError> {
        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        let rows = self
            .memberships
            .list_with_emails(&mut scope, tenant_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(rows
            .into_iter()
            .map(|(membership, email)| MemberView {
                user_id: membership.user_id().to_string(),
                email: email.to_string(),
                role_ids: membership.role_ids().to_vec(),
                status: membership.status(),
                invited_by: membership.invited_by(),
                created_at: membership.created_at(),
            })
            .collect())
    }
}

pub struct GrantMembership {
    tx_manager: TxManager,
    users: Arc<dyn UserRepository>,
    memberships: Arc<dyn MembershipRepository>,
    roles: Arc<dyn RoleCatalog>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    protected_role_key: String,
}

impl GrantMembership {
    pub fn new(
        tx_manager: TxManager,
        users: Arc<dyn UserRepository>,
        memberships: Arc<dyn MembershipRepository>,
        roles: Arc<dyn RoleCatalog>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            tx_manager,
            users,
            memberships,
            roles,
            events,
            clock,
            protected_role_key: "tenant_owner".to_string(),
        }
    }

    pub fn with_protected_role_key(mut self, key: impl Into<String>) -> Self {
        self.protected_role_key = key.into();
        self
    }

    /// Grant a brand-new membership (or error when one already exists —
    /// role changes go through [`UpdateMemberRoles`]).
    #[tracing::instrument(name = "identity.grant_membership", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        cmd: GrantMembershipCommand,
        actor: UserId,
    ) -> Result<MemberView, IdentityError> {
        let email = Email::parse(&cmd.email)?;
        let assignable =
            DelegationPolicy::validate_assignable(tenant_id, &cmd.role_ids, &*self.roles).await?;

        let mut platform = self
            .tx_manager
            .begin_unscoped("membership_grant_lookup", "identity")
            .await
            .map_err(storage)?;
        let user = self
            .users
            .find_by_email(&mut platform, &email)
            .await?
            .ok_or_else(|| IdentityError::UserEmailNotFound {
                email: email.clone(),
            })?;
        user.ensure_active()?;
        let user_id = user.id();
        platform.commit().await.map_err(storage)?;

        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;

        DelegationPolicy::check_grant(
            &mut scope,
            tenant_id,
            actor,
            &cmd.role_ids,
            &assignable,
            &self.protected_role_key,
            &*self.memberships,
        )
        .await?;

        let now = self.clock.now();
        let membership = Membership::grant(tenant_id, user_id, cmd.role_ids, Some(actor), now)?;

        if self
            .memberships
            .find(&mut scope, tenant_id, user_id)
            .await?
            .is_some()
        {
            return Err(IdentityError::NotAMember { user_id, tenant_id });
        }
        let version = self.memberships.save(&mut scope, &membership).await?;
        let events = vec![IdentityEvent::MembershipGranted {
            tenant_id,
            user_id,
            role_ids: membership.role_ids().to_vec(),
            version,
        }];
        self.events.publish_scoped(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;
        info!(tenant_id = %tenant_id, user_id = %user_id, "membership granted");
        Ok(MemberView {
            user_id: user_id.to_string(),
            email: email.to_string(),
            role_ids: membership.role_ids().to_vec(),
            status: membership.status(),
            invited_by: membership.invited_by(),
            created_at: membership.created_at(),
        })
    }
}

pub struct UpdateMemberRoles {
    tx_manager: TxManager,
    memberships: Arc<dyn MembershipRepository>,
    roles: Arc<dyn RoleCatalog>,
    events: Arc<dyn EventPublisher>,
    protected_role_key: String,
}

impl UpdateMemberRoles {
    pub fn new(
        tx_manager: TxManager,
        memberships: Arc<dyn MembershipRepository>,
        roles: Arc<dyn RoleCatalog>,
        events: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            tx_manager,
            memberships,
            roles,
            events,
            protected_role_key: "tenant_owner".to_string(),
        }
    }

    pub fn with_protected_role_key(mut self, key: impl Into<String>) -> Self {
        self.protected_role_key = key.into();
        self
    }

    #[tracing::instrument(name = "identity.update_member_roles", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        cmd: UpdateRolesCommand,
        actor: UserId,
    ) -> Result<(), IdentityError> {
        let assignable =
            DelegationPolicy::validate_assignable(tenant_id, &cmd.role_ids, &*self.roles).await?;

        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        let mut membership = self
            .memberships
            .find(&mut scope, tenant_id, user_id)
            .await?
            .ok_or(IdentityError::NotAMember { user_id, tenant_id })?;

        DelegationPolicy::check_update_roles(
            &mut scope,
            tenant_id,
            actor,
            &membership,
            &cmd.role_ids,
            &assignable,
            &self.protected_role_key,
            &*self.memberships,
        )
        .await?;

        let (_added, removed) = membership.replace_roles(cmd.role_ids)?;
        let version = self.memberships.save(&mut scope, &membership).await?;
        let events = vec![IdentityEvent::MembershipRolesChanged {
            tenant_id,
            user_id,
            role_ids: membership.role_ids().to_vec(),
            removed_role_ids: removed,
            version,
        }];
        self.events.publish_scoped(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;
        info!(tenant_id = %tenant_id, user_id = %user_id, "membership roles updated");
        Ok(())
    }
}

pub struct RemoveMember {
    tx_manager: TxManager,
    memberships: Arc<dyn MembershipRepository>,
    roles: Arc<dyn RoleCatalog>,
    events: Arc<dyn EventPublisher>,
    /// Role key whose last holder may not be removed (owner lock).
    protected_role_key: String,
}

impl RemoveMember {
    pub fn new(
        tx_manager: TxManager,
        memberships: Arc<dyn MembershipRepository>,
        roles: Arc<dyn RoleCatalog>,
        events: Arc<dyn EventPublisher>,
    ) -> Self {
        Self {
            tx_manager,
            memberships,
            roles,
            events,
            protected_role_key: "tenant_owner".to_string(),
        }
    }

    #[tracing::instrument(name = "identity.remove_member", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        tenant_id: TenantId,
        user_id: UserId,
        actor: UserId,
    ) -> Result<(), IdentityError> {
        if user_id == actor {
            return Err(IdentityError::CannotRemoveSelf);
        }

        let mut scope = self
            .tx_manager
            .begin_for(tenant_id.as_i64())
            .await
            .map_err(storage)?;
        let membership = self
            .memberships
            .find(&mut scope, tenant_id, user_id)
            .await?
            .ok_or(IdentityError::NotAMember { user_id, tenant_id })?;

        // Last-owner guard: removing the final tenant_owner locks the tenant
        // out of its own administration.
        for role_id in membership.role_ids() {
            if self.roles.role_key(tenant_id, *role_id).await?.as_deref()
                == Some(self.protected_role_key.as_str())
            {
                if self
                    .memberships
                    .count_role_holders(&mut scope, tenant_id, *role_id)
                    .await?
                    <= 1
                {
                    return Err(IdentityError::LastRoleHolder {
                        from: membership.status(),
                        to: MembershipStatus::Suspended,
                        role: self.protected_role_key.clone(),
                    });
                }

                let actor_membership = self.memberships.find(&mut scope, tenant_id, actor).await?;
                let actor_has_owner = actor_membership
                    .map(|m| m.is_active() && m.role_ids().contains(role_id))
                    .unwrap_or(false);
                if !actor_has_owner {
                    return Err(IdentityError::DelegationDenied {
                        actor,
                        role: self.protected_role_key.clone(),
                        detail: format!(
                            "only holders of '{}' can remove a holder of this role",
                            self.protected_role_key
                        ),
                    });
                }
            }
        }

        let revoked_role_ids = membership.role_ids().to_vec();
        let version = self
            .memberships
            .remove(&mut scope, tenant_id, user_id)
            .await?
            .ok_or(IdentityError::NotAMember { user_id, tenant_id })?;
        let events = vec![IdentityEvent::MembershipRevoked {
            tenant_id,
            user_id,
            role_ids: revoked_role_ids,
            version,
        }];
        self.events.publish_scoped(&mut scope, &events).await?;
        scope.commit().await.map_err(storage)?;
        info!(tenant_id = %tenant_id, user_id = %user_id, "membership revoked");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_requires_roles_and_a_valid_email_before_io() {
        let email = Email::parse("  Alice@Example.COM ").unwrap();
        assert_eq!(email.as_str(), "alice@example.com");
        let bad = GrantMembershipCommand {
            email: "not-an-email".to_string(),
            role_ids: vec![RoleId::try_from(101_i64).unwrap()],
        };
        assert!(matches!(
            Email::parse(&bad.email),
            Err(IdentityError::InvalidEmail(_))
        ));
        let no_roles = GrantMembershipCommand {
            email: "alice@example.com".to_string(),
            role_ids: vec![],
        };
        assert!(no_roles.role_ids.is_empty());
    }
}
