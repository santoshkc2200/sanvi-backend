//! Tenant-scoped invitation storage (`identity.invitations`).

use async_trait::async_trait;
use sanvi_persistence::{PlatformScope, ScopedTx};
use sqlx::FromRow;
use sanvi_kernel::{RoleId, TenantId};

use crate::domain::invitation::{Invitation, InvitationId};
use crate::domain::user::UserId;
use crate::domain::values::Email;
use crate::domain::{IdentityError, InvitationRepository};

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxInvitationRepository;

impl SqlxInvitationRepository {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Clone, FromRow)]
struct InvitationRow {
    id: i64,
    tenant_id: i64,
    email: String,
    role_ids: Vec<i64>,
    token_hash: String,
    invited_by: Option<i64>,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
    accepted_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn invitation_from_row(row: InvitationRow) -> Result<Invitation, IdentityError> {
    Ok(Invitation::from_parts(
        InvitationId::try_from(row.id).map_err(|err| IdentityError::Storage(format!("invalid invitation id: {err}")))?,
        TenantId::try_from(row.tenant_id).map_err(|err| IdentityError::Storage(format!("invalid tenant id: {err}")))?,
        Email::parse(&row.email)?,
        row.role_ids.into_iter().map(RoleId::try_from).collect::<Result<Vec<_>, _>>().map_err(|err| IdentityError::Storage(format!("invalid role id: {err}")))?,
        row.token_hash,
        row.invited_by.map(UserId::try_from).transpose().map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
        row.created_at,
        row.expires_at,
        row.accepted_at,
    ))
}

#[async_trait]
impl InvitationRepository for SqlxInvitationRepository {
    async fn save(
        &self,
        scope: &mut ScopedTx<'_>,
        invitation: &Invitation,
    ) -> Result<(), IdentityError> {
        sqlx::query!(
            "INSERT INTO identity.invitations
               (id, tenant_id, email, role_ids, token_hash, invited_by, created_at,
                expires_at, accepted_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET
               email = EXCLUDED.email,
               role_ids = EXCLUDED.role_ids,
               token_hash = EXCLUDED.token_hash,
               expires_at = EXCLUDED.expires_at,
               accepted_at = EXCLUDED.accepted_at",
            invitation.id().as_i64(),
            invitation.tenant_id().as_i64(),
            invitation.email().as_str(),
            &invitation.role_ids().iter().map(|id| id.as_i64()).collect::<Vec<_>>(),
            invitation.token_hash(),
            invitation.invited_by().map(|id| id.as_i64()),
            invitation.created_at(),
            invitation.expires_at(),
            invitation.accepted_at(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn find_by_token_hash(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        token_hash: &str,
    ) -> Result<Option<Invitation>, IdentityError> {
        let row = sqlx::query_as!(
            InvitationRow,
            "SELECT id, tenant_id, email, role_ids, token_hash, invited_by, created_at,
                    expires_at, accepted_at
             FROM identity.invitations
             WHERE tenant_id = $1 AND token_hash = $2",
            tenant_id.as_i64(),
            token_hash,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        row.map(invitation_from_row).transpose()
    }

    async fn find_by_id(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: InvitationId,
    ) -> Result<Option<Invitation>, IdentityError> {
        let row = sqlx::query_as!(
            InvitationRow,
            "SELECT id, tenant_id, email, role_ids, token_hash, invited_by, created_at,
                    expires_at, accepted_at
             FROM identity.invitations
             WHERE tenant_id = $1 AND id = $2",
            tenant_id.as_i64(),
            id.as_i64(),
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        row.map(invitation_from_row).transpose()
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
    ) -> Result<Vec<Invitation>, IdentityError> {
        let rows = sqlx::query_as!(
            InvitationRow,
            "SELECT id, tenant_id, email, role_ids, token_hash, invited_by, created_at,
                    expires_at, accepted_at
             FROM identity.invitations
             WHERE tenant_id = $1 ORDER BY created_at",
            tenant_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        rows.into_iter().map(invitation_from_row).collect()
    }

    async fn remove_by_email_platform(
        &self,
        scope: &mut PlatformScope<'_>,
        email: &str,
    ) -> Result<usize, IdentityError> {
        let result = sqlx::query("DELETE FROM identity.invitations WHERE email = $1")
            .bind(email.to_ascii_lowercase())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(result.rows_affected() as usize)
    }

    async fn remove(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        id: InvitationId,
    ) -> Result<(), IdentityError> {
        sqlx::query!(
            "DELETE FROM identity.invitations WHERE tenant_id = $1 AND id = $2",
            tenant_id.as_i64(),
            id.as_i64(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }
}
