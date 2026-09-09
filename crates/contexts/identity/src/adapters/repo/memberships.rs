//! Tenant-scoped membership storage (`identity.memberships` +
//! `identity.membership_roles`). Every method requires a `&mut ScopedTx`:
//! forgetting tenant scoping here is a type error.

use async_trait::async_trait;
use sanvi_kernel::{RoleId, TenantId};
use sanvi_persistence::{PlatformScope, ScopedTx};

use crate::domain::membership::{Membership, MembershipStatus};
use crate::domain::user::UserId;
use crate::domain::values::Email;
use crate::domain::{IdentityError, MembershipRepository};

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxMembershipRepository;

impl SqlxMembershipRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MembershipRepository for SqlxMembershipRepository {
    async fn save(
        &self,
        scope: &mut ScopedTx<'_>,
        membership: &Membership,
    ) -> Result<i64, IdentityError> {
        // `version + 1` is evaluated under this statement's row lock, so
        // concurrent writers serialise and every save observes a strictly
        // greater value than the one before it.
        let version = sqlx::query_scalar!(
            "INSERT INTO identity.memberships (tenant_id, user_id, status, invited_by, created_at, version)
             VALUES ($1, $2, $3, $4, $5, 1)
             ON CONFLICT (tenant_id, user_id) DO UPDATE SET
               status = EXCLUDED.status,
               invited_by = EXCLUDED.invited_by,
               version = identity.memberships.version + 1
             RETURNING version",
            membership.tenant_id().as_i64(),
            membership.user_id().as_i64(),
            membership.status().as_str(),
            membership.invited_by().map(|id| id.as_i64()),
            membership.created_at(),
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        sqlx::query!(
            "DELETE FROM identity.membership_roles WHERE tenant_id = $1 AND user_id = $2",
            membership.tenant_id().as_i64(),
            membership.user_id().as_i64(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        for role_id in membership.role_ids() {
            sqlx::query!(
                "INSERT INTO identity.membership_roles (tenant_id, user_id, role_id)
                 VALUES ($1, $2, $3)",
                membership.tenant_id().as_i64(),
                membership.user_id().as_i64(),
                role_id.as_i64(),
            )
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        }
        Ok(version)
    }

    async fn find(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<Option<Membership>, IdentityError> {
        let membership = sqlx::query!(
            "SELECT tenant_id, user_id, status, invited_by, created_at
             FROM identity.memberships WHERE tenant_id = $1 AND user_id = $2",
            tenant_id.as_i64(),
            user_id.as_i64(),
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let Some(row) = membership else {
            return Ok(None);
        };
        let roles = self.role_ids_for(scope, tenant_id, user_id).await?;
        Ok(Some(Membership::from_parts(
            TenantId::try_from(row.tenant_id)
                .map_err(|err| IdentityError::Storage(format!("invalid tenant id: {err}")))?,
            UserId::try_from(row.user_id)
                .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
            roles,
            MembershipStatus::parse(&row.status)?,
            row.invited_by
                .map(UserId::try_from)
                .transpose()
                .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
            row.created_at,
        )))
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Membership>, IdentityError> {
        let rows = sqlx::query!(
            "SELECT tenant_id, user_id, status, invited_by, created_at
             FROM identity.memberships WHERE tenant_id = $1 ORDER BY created_at",
            tenant_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let mut memberships = Vec::with_capacity(rows.len());
        for row in rows {
            let user_id = UserId::try_from(row.user_id)
                .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?;
            let roles = self.role_ids_for(scope, tenant_id, user_id).await?;
            memberships.push(Membership::from_parts(
                tenant_id,
                user_id,
                roles,
                MembershipStatus::parse(&row.status)?,
                row.invited_by
                    .map(UserId::try_from)
                    .transpose()
                    .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
                row.created_at,
            ));
        }
        Ok(memberships)
    }

    async fn list_with_emails(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<(Membership, Email)>, IdentityError> {
        let rows = sqlx::query!(
            "SELECT m.tenant_id, m.user_id, m.status, m.invited_by, m.created_at, u.primary_email
             FROM identity.memberships m
             JOIN identity.users u ON u.id = m.user_id
             WHERE m.tenant_id = $1 ORDER BY m.created_at",
            tenant_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            let user_id = UserId::try_from(row.user_id)
                .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?;
            let roles = self.role_ids_for(scope, tenant_id, user_id).await?;
            let email = Email::parse(&row.primary_email)?;
            result.push((
                Membership::from_parts(
                    tenant_id,
                    user_id,
                    roles,
                    MembershipStatus::parse(&row.status)?,
                    row.invited_by
                        .map(UserId::try_from)
                        .transpose()
                        .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
                    row.created_at,
                ),
                email,
            ));
        }
        Ok(result)
    }

    async fn remove(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<Option<i64>, IdentityError> {
        let version = sqlx::query_scalar!(
            "DELETE FROM identity.memberships WHERE tenant_id = $1 AND user_id = $2
             RETURNING version + 1",
            tenant_id.as_i64(),
            user_id.as_i64(),
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(version.flatten())
    }

    async fn remove_all_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, IdentityError> {
        sqlx::query("DELETE FROM identity.membership_roles WHERE user_id = $1")
            .bind(user_id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        let result = sqlx::query("DELETE FROM identity.memberships WHERE user_id = $1")
            .bind(user_id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(result.rows_affected() as usize)
    }

    async fn list_all_platform(
        &self,
        scope: &mut PlatformScope<'_>,
    ) -> Result<Vec<Membership>, IdentityError> {
        let rows = sqlx::query!(
            "SELECT tenant_id, user_id, status, invited_by, created_at
             FROM identity.memberships ORDER BY tenant_id, created_at",
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let role_rows =
            sqlx::query!("SELECT tenant_id, user_id, role_id FROM identity.membership_roles",)
                .fetch_all(&mut **scope.tx_mut().as_inner())
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let mut roles_by_member: std::collections::HashMap<(i64, i64), Vec<i64>> =
            std::collections::HashMap::new();
        for row in role_rows {
            roles_by_member
                .entry((row.tenant_id, row.user_id))
                .or_default()
                .push(row.role_id);
        }

        rows.into_iter()
            .map(|row| {
                let tenant_id = TenantId::try_from(row.tenant_id)
                    .map_err(|err| IdentityError::Storage(format!("invalid tenant id: {err}")))?;
                let user_id = UserId::try_from(row.user_id)
                    .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?;
                let roles = roles_by_member
                    .remove(&(row.tenant_id, row.user_id))
                    .unwrap_or_default()
                    .into_iter()
                    .map(RoleId::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|err| IdentityError::Storage(format!("invalid role id: {err}")))?;
                Ok(Membership::from_parts(
                    tenant_id,
                    user_id,
                    roles,
                    MembershipStatus::parse(&row.status)?,
                    row.invited_by
                        .map(UserId::try_from)
                        .transpose()
                        .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
                    row.created_at,
                ))
            })
            .collect()
    }

    async fn list_for_user_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<Vec<Membership>, IdentityError> {
        let rows = sqlx::query!(
            "SELECT tenant_id, user_id, status, invited_by, created_at
             FROM identity.memberships WHERE user_id = $1 ORDER BY created_at",
            user_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let role_rows = sqlx::query!(
            "SELECT tenant_id, role_id FROM identity.membership_roles WHERE user_id = $1",
            user_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        let mut roles_by_tenant: std::collections::HashMap<i64, Vec<i64>> =
            std::collections::HashMap::new();
        for row in role_rows {
            roles_by_tenant
                .entry(row.tenant_id)
                .or_default()
                .push(row.role_id);
        }

        rows.into_iter()
            .map(|row| {
                let tenant_id = TenantId::try_from(row.tenant_id)
                    .map_err(|err| IdentityError::Storage(format!("invalid tenant id: {err}")))?;
                let roles = roles_by_tenant
                    .remove(&row.tenant_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(RoleId::try_from)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|err| IdentityError::Storage(format!("invalid role id: {err}")))?;
                Ok(Membership::from_parts(
                    tenant_id,
                    UserId::try_from(row.user_id)
                        .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
                    roles,
                    MembershipStatus::parse(&row.status)?,
                    row.invited_by
                        .map(UserId::try_from)
                        .transpose()
                        .map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
                    row.created_at,
                ))
            })
            .collect()
    }

    async fn count_members_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        tenant_id: TenantId,
    ) -> Result<u32, IdentityError> {
        let count = sqlx::query!(
            "SELECT count(*)::int4 AS count FROM identity.memberships
             WHERE tenant_id = $1 AND status = 'active'",
            tenant_id.as_i64(),
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(count.count.unwrap_or(0) as u32)
    }

    async fn count_role_holders(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        role_id: RoleId,
    ) -> Result<u32, IdentityError> {
        let count = sqlx::query!(
            "SELECT count(*)::int4 AS count FROM identity.membership_roles
             WHERE tenant_id = $1 AND role_id = $2",
            tenant_id.as_i64(),
            role_id.as_i64(),
        )
        .fetch_one(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(count.count.unwrap_or(0) as u32)
    }
}

impl SqlxMembershipRepository {
    async fn role_ids_for(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        user_id: UserId,
    ) -> Result<Vec<RoleId>, IdentityError> {
        let rows = sqlx::query!(
            "SELECT role_id FROM identity.membership_roles
             WHERE tenant_id = $1 AND user_id = $2 ORDER BY role_id",
            tenant_id.as_i64(),
            user_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        rows.into_iter()
            .map(|row| RoleId::try_from(row.role_id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| IdentityError::Storage(format!("invalid role id: {err}")))
    }
}
