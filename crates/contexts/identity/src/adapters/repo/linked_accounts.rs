//! Linked-account storage (`identity.linked_accounts`): the global
//! `(provider, subject)` → user binding.

use async_trait::async_trait;
use sanvi_persistence::PlatformScope;

use crate::domain::user::{LinkedAccount, LinkedVia, UserId};
use crate::domain::values::{ProviderId, ProviderSubject};
use crate::domain::{IdentityError, LinkedAccountRepository};

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxLinkedAccountRepository;

impl SqlxLinkedAccountRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LinkedAccountRepository for SqlxLinkedAccountRepository {
    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        provider: &ProviderId,
        subject: &ProviderSubject,
    ) -> Result<Option<LinkedAccount>, IdentityError> {
        let row = sqlx::query!(
            "SELECT user_id, provider, subject, linked_via, linked_at
             FROM identity.linked_accounts WHERE provider = $1 AND subject = $2",
            provider.as_str(),
            subject.as_str(),
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        row.map(|row| {
            Ok(LinkedAccount::new(
                UserId::try_from(row.user_id).map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
                ProviderId::parse(&row.provider)?,
                ProviderSubject::new(row.subject),
                LinkedVia::parse(&row.linked_via)?,
                row.linked_at,
            ))
        })
        .transpose()
    }

    async fn link(
        &self,
        scope: &mut PlatformScope<'_>,
        account: &LinkedAccount,
    ) -> Result<(), IdentityError> {
        sqlx::query!(
            "INSERT INTO identity.linked_accounts (user_id, provider, subject, linked_via, linked_at)
             VALUES ($1, $2, $3, $4, $5)",
            account.user_id().as_i64(),
            account.provider().as_str(),
            account.subject().as_str(),
            account.linked_via().as_str(),
            account.linked_at(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn remove_all(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<usize, IdentityError> {
        let result = sqlx::query("DELETE FROM identity.linked_accounts WHERE user_id = $1")
            .bind(user_id.as_i64())
            .execute(&mut **scope.tx_mut().as_inner())
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(result.rows_affected() as usize)
    }

    async fn list_for_user(
        &self,
        scope: &mut PlatformScope<'_>,
        user_id: UserId,
    ) -> Result<Vec<LinkedAccount>, IdentityError> {
        let rows = sqlx::query!(
            "SELECT user_id, provider, subject, linked_via, linked_at
             FROM identity.linked_accounts WHERE user_id = $1 ORDER BY linked_at",
            user_id.as_i64(),
        )
        .fetch_all(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        rows.into_iter()
            .map(|row| {
                Ok(LinkedAccount::new(
                    UserId::try_from(row.user_id).map_err(|err| IdentityError::Storage(format!("invalid user id: {err}")))?,
                    ProviderId::parse(&row.provider)?,
                    ProviderSubject::new(row.subject),
                    LinkedVia::parse(&row.linked_via)?,
                    row.linked_at,
                ))
            })
            .collect()
    }
}
