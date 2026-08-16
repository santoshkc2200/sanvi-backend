//! Link-intent storage (`identity.link_intents`): single-use rows consumed
//! atomically.

use async_trait::async_trait;
use sanvi_persistence::PlatformScope;

use crate::domain::link_intent::{LinkIntent, LinkIntentPayload};
use crate::domain::{IdentityError, LinkIntentRepository};

#[derive(Debug, Clone, Copy, Default)]
pub struct SqlxLinkIntentRepository;

impl SqlxLinkIntentRepository {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LinkIntentRepository for SqlxLinkIntentRepository {
    async fn save(
        &self,
        scope: &mut PlatformScope<'_>,
        intent: &LinkIntent,
    ) -> Result<(), IdentityError> {
        let payload = serde_json::to_value(intent.payload())
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        sqlx::query!(
            "INSERT INTO identity.link_intents (nonce_hash, payload, expires_at, created_at)
             VALUES ($1, $2, $3, $4)",
            intent.nonce_hash(),
            payload,
            intent.expires_at(),
            intent.created_at(),
        )
        .execute(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn find(
        &self,
        scope: &mut PlatformScope<'_>,
        nonce_hash: &str,
    ) -> Result<Option<LinkIntent>, IdentityError> {
        let row = sqlx::query!(
            "SELECT nonce_hash, payload, expires_at, created_at
             FROM identity.link_intents WHERE nonce_hash = $1",
            nonce_hash,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        row.map(|row| {
            let payload: LinkIntentPayload = serde_json::from_value(row.payload)
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
            Ok(LinkIntent::from_parts(
                row.nonce_hash,
                payload,
                row.expires_at,
                row.created_at,
            ))
        })
        .transpose()
    }

    async fn take(
        &self,
        scope: &mut PlatformScope<'_>,
        nonce_hash: &str,
    ) -> Result<Option<LinkIntent>, IdentityError> {
        let row = sqlx::query!(
            "DELETE FROM identity.link_intents WHERE nonce_hash = $1
             RETURNING nonce_hash, payload, expires_at, created_at",
            nonce_hash,
        )
        .fetch_optional(&mut **scope.tx_mut().as_inner())
        .await
        .map_err(|err| IdentityError::Storage(err.to_string()))?;

        row.map(|row| {
            let payload: LinkIntentPayload = serde_json::from_value(row.payload)
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
            Ok(LinkIntent::from_parts(
                row.nonce_hash,
                payload,
                row.expires_at,
                row.created_at,
            ))
        })
        .transpose()
    }
}
