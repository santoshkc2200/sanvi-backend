//! Tenant-scoped invitation storage (`identity.invitations`).

use async_trait::async_trait;
use sanvi_persistence::{PlatformScope, ScopedTx};
use sqlx::FromRow;
use uuid::Uuid;

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
    id: Uuid,
    tenant_id: Uuid,
    email: String,
    role_ids: Vec<Uuid>,
    token_hash: String,
    invited_by: Option<Uuid>,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
    accepted_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn invitation_from_row(row: InvitationRow) -> Result<Invitation, IdentityError> {
    Ok(Invitation::from_parts(
        InvitationId::from_uuid(row.id),
        row.tenant_id,
        Email::parse(&row.email)?,
        row.role_ids,
        row.token_hash,
        row.invited_by.map(UserId::from_uuid),
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
            invitation.id().as_uuid(),
            invitation.tenant_id(),
            invitation.email().as_str(),
            invitation.role_ids(),
            invitation.token_hash(),
            invitation.invited_by().map(|id| id.as_uuid()),
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
        tenant_id: Uuid,
        token_hash: &str,
    ) -> Result<Option<Invitation>, IdentityError> {
        let row = sqlx::query_as!(
            InvitationRow,
            "SELECT id, tenant_id, email, role_ids, token_hash, invited_by, created_at,
                    expires_at, accepted_at
             FROM identity.invitations
             WHERE tenant_id = $1 AND token_hash = $2",
            tenant_id,
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
        tenant_id: Uuid,
        id: InvitationId,
    ) -> Result<Option<Invitation>, IdentityError> {
        let row = sqlx::query_as!(
            InvitationRow,
            "SELECT id, tenant_id, email, role_ids, token_hash, invited_by, created_at,
                    expires_at, accepted_at
             FROM identity.invitations
             WHERE tenant_id = $1 AND id = $2",
            tenant_id,
            id.as_uuid(),
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        row.map(invitation_from_row).transpose()
    }

    async fn list(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<Invitation>, IdentityError> {
        let rows = sqlx::query_as!(
            InvitationRow,
            "SELECT id, tenant_id, email, role_ids, token_hash, invited_by, created_at,
                    expires_at, accepted_at
             FROM identity.invitations
             WHERE tenant_id = $1 ORDER BY created_at",
            tenant_id,
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
        tenant_id: Uuid,
        id: InvitationId,
    ) -> Result<(), IdentityError> {
        sqlx::query!(
            "DELETE FROM identity.invitations WHERE tenant_id = $1 AND id = $2",
            tenant_id,
            id.as_uuid(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }
}
