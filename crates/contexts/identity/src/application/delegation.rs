use std::collections::BTreeSet;

use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::ScopedTx;

use crate::domain::{IdentityError, Membership, MembershipRepository, RoleCatalog, UserId};

/// Actor-aware delegation policy enforcing role assignability, ownership transfer limits,
/// and last-owner preservation.
pub struct DelegationPolicy;

impl DelegationPolicy {
    /// Validates that `role_ids` is non-empty and all IDs are assignable in `tenant_id`.
    /// Returns the resolved assignable roles `(RoleId, role_key)`.
    #[tracing::instrument(name = "identity.validate_assignable", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn validate_assignable(
        tenant_id: TenantId,
        role_ids: &[RoleId],
        roles: &dyn RoleCatalog,
    ) -> Result<Vec<(RoleId, String)>, IdentityError> {
        if role_ids.is_empty() {
            return Err(IdentityError::NoRoles);
        }
        let assignable = roles.assignable_roles(tenant_id).await?;
        let assignable_ids: BTreeSet<RoleId> = assignable.iter().map(|(id, _)| *id).collect();
        for id in role_ids {
            if !assignable_ids.contains(id) {
                return Err(IdentityError::RoleNotAssignable(*id));
            }
        }
        Ok(assignable)
    }

    /// Checks that `actor` has authority to grant `target_roles`.
    /// If `target_roles` includes `protected_role_key`, `actor` must
    /// be an active member holding `protected_role_key` in `tenant_id`,
    /// unless no holders currently exist in the tenant (initial bootstrap).
    #[tracing::instrument(name = "identity.check_grant", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn check_grant(
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        actor: UserId,
        target_roles: &[RoleId],
        assignable: &[(RoleId, String)],
        protected_role_key: &str,
        memberships: &dyn MembershipRepository,
    ) -> Result<(), IdentityError> {
        let owner_assigned = assignable
            .iter()
            .find(|(id, k)| k == protected_role_key && target_roles.contains(id))
            .map(|(id, _)| *id);
        if let Some(owner_role_id) = owner_assigned {
            let owner_count = memberships
                .count_role_holders(scope, tenant_id, owner_role_id)
                .await?;
            if owner_count > 0 {
                let actor_membership = memberships.find(scope, tenant_id, actor).await?;
                let actor_has_owner = actor_membership
                    .map(|m| m.is_active() && m.role_ids().contains(&owner_role_id))
                    .unwrap_or(false);
                if !actor_has_owner {
                    return Err(IdentityError::DelegationDenied {
                        actor,
                        role: protected_role_key.to_string(),
                        detail: format!(
                            "only holders of '{protected_role_key}' can assign this role"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Checks that `actor` has authority to invite with `target_roles`.
    #[tracing::instrument(name = "identity.check_invite", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn check_invite(
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        actor: UserId,
        target_roles: &[RoleId],
        assignable: &[(RoleId, String)],
        protected_role_key: &str,
        memberships: &dyn MembershipRepository,
    ) -> Result<(), IdentityError> {
        Self::check_grant(
            scope,
            tenant_id,
            actor,
            target_roles,
            assignable,
            protected_role_key,
            memberships,
        )
        .await
    }

    /// Checks that when accepting an invitation with `target_roles`, the invitation's creator
    /// (`invited_by`) was authorized to grant those roles.
    #[tracing::instrument(name = "identity.check_accept", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn check_accept(
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        invited_by: Option<UserId>,
        target_roles: &[RoleId],
        assignable: &[(RoleId, String)],
        protected_role_key: &str,
        memberships: &dyn MembershipRepository,
    ) -> Result<(), IdentityError> {
        let owner_assigned = assignable
            .iter()
            .find(|(id, k)| k == protected_role_key && target_roles.contains(id))
            .map(|(id, _)| *id);
        if let Some(owner_role_id) = owner_assigned {
            let owner_count = memberships
                .count_role_holders(scope, tenant_id, owner_role_id)
                .await?;
            if owner_count > 0 {
                let inviter = invited_by.ok_or_else(|| IdentityError::DelegationDenied {
                    actor: UserId::try_from(0_i64).unwrap(),
                    role: protected_role_key.to_string(),
                    detail: "invitation for protected role lacks authorized inviter".to_string(),
                })?;
                let inviter_membership = memberships.find(scope, tenant_id, inviter).await?;
                let inviter_has_owner = inviter_membership
                    .map(|m| m.is_active() && m.role_ids().contains(&owner_role_id))
                    .unwrap_or(false);
                if !inviter_has_owner {
                    return Err(IdentityError::DelegationDenied {
                        actor: inviter,
                        role: protected_role_key.to_string(),
                        detail:
                            "inviter no longer holds required authority to grant protected role"
                                .to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Checks role replacement in `UpdateMemberRoles`:
    /// - Checks delegation limits (only protected role holder can add/modify/remove protected role)
    /// - Preserves last-owner invariant if protected role is being removed.
    #[tracing::instrument(name = "identity.check_update_roles", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn check_update_roles(
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        actor: UserId,
        target_membership: &Membership,
        new_roles: &[RoleId],
        assignable: &[(RoleId, String)],
        protected_role_key: &str,
        memberships: &dyn MembershipRepository,
    ) -> Result<(), IdentityError> {
        if let Some((owner_role_id, _)) = assignable.iter().find(|(_, k)| k == protected_role_key) {
            let was_owner = target_membership.role_ids().contains(owner_role_id);
            let will_be_owner = new_roles.contains(owner_role_id);

            if was_owner || will_be_owner {
                let owner_count = memberships
                    .count_role_holders(scope, tenant_id, *owner_role_id)
                    .await?;

                if owner_count > 0 {
                    let actor_membership = memberships.find(scope, tenant_id, actor).await?;
                    let actor_has_owner = actor_membership
                        .map(|m| m.is_active() && m.role_ids().contains(owner_role_id))
                        .unwrap_or(false);
                    if !actor_has_owner {
                        return Err(IdentityError::DelegationDenied {
                            actor,
                            role: protected_role_key.to_string(),
                            detail: format!(
                                "only holders of '{protected_role_key}' can assign, modify, or revoke this role"
                            ),
                        });
                    }
                }

                if was_owner && !will_be_owner && owner_count <= 1 {
                    return Err(IdentityError::LastRoleHolder {
                        from: target_membership.status(),
                        to: target_membership.status(),
                        role: protected_role_key.to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}
