//! Platform-scoped user projection storage (`identity.users`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_persistence::PlatformScope;
use sqlx::FromRow;
use uuid::Uuid;

use crate::domain::user::{User, UserId, UserStatus};
use crate::domain::values::{Email, Locale};
use crate::domain::{IdentityError, UserRepository};

#[derive(Debug, Clone, FromRow)]
struct UserRow {
    id: Uuid,
    kratos_identity_id: Uuid,
    primary_email: String,
    email_verified: bool,
    status: String,
    locale: Option<String>,
    created_at: DateTime<Utc>,
    deactivated_at: Option<DateTime<Utc>>,
}

impl UserRow {
    fn into_user(self) -> Result<User, IdentityError> {
        let locale = match self.locale {
            Some(locale) => Some(Locale::parse(&locale)?),
            None => None,
        };
        Ok(User::from_parts(
            UserId::from_uuid(self.id),
            self.kratos_identity_id,
            Email::parse(&self.primary_email)?,
            self.email_verified,
            UserStatus::parse(&self.status)?,
            locale,
            self.created_at,
            self.deactivated_at,
        ))
    }
}

/// Platform-scoped repository: users exist across tenants, so every method
/// requires the audited `&mut PlatformScope`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxUserRepository;

impl SqlxUserRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl UserRepository for SqlxUserRepository {
    async fn save(&self, scope: &mut PlatformScope<'_>, user: &User) -> Result<(), IdentityError> {
        sqlx::query!(
            "INSERT INTO identity.users
               (id, kratos_identity_id, primary_email, email_verified, status, locale,
                created_at, deactivated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (id) DO UPDATE SET
               kratos_identity_id = EXCLUDED.kratos_identity_id,
               primary_email = EXCLUDED.primary_email,
               email_verified = EXCLUDED.email_verified,
               status = EXCLUDED.status,
               locale = EXCLUDED.locale,
               deactivated_at = EXCLUDED.deactivated_at",
            user.id().as_uuid(),
            user.kratos_identity_id(),
            user.primary_email().as_str(),
            user.email_verified(),
            user.status().as_str(),
            user.locale().map(|locale| locale.as_str()),
            user.created_at(),
            user.deactivated_at(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn find_by_id(
        &self,
        scope: &mut PlatformScope<'_>,
        id: UserId,
    ) -> Result<Option<User>, IdentityError> {
        let row = sqlx::query_as!(
            UserRow,
            "SELECT id, kratos_identity_id, primary_email, email_verified, status, locale,
                    created_at, deactivated_at
             FROM identity.users WHERE id = $1",
            id.as_uuid()
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        row.map(UserRow::into_user).transpose()
    }

    async fn find_by_kratos(
        &self,
        scope: &mut PlatformScope<'_>,
        kratos_identity_id: Uuid,
    ) -> Result<Option<User>, IdentityError> {
        let row = sqlx::query_as!(
            UserRow,
            "SELECT id, kratos_identity_id, primary_email, email_verified, status, locale,
                    created_at, deactivated_at
             FROM identity.users WHERE kratos_identity_id = $1",
            kratos_identity_id
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        row.map(UserRow::into_user).transpose()
    }

    async fn find_by_email(
        &self,
        scope: &mut PlatformScope<'_>,
        email: &Email,
    ) -> Result<Option<User>, IdentityError> {
        let row = sqlx::query_as!(
            UserRow,
            "SELECT id, kratos_identity_id, primary_email, email_verified, status, locale,
                    created_at, deactivated_at
             FROM identity.users
             WHERE primary_email = $1 AND deactivated_at IS NULL",
            email.as_str()
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        row.map(UserRow::into_user).transpose()
    }
}
