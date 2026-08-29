//! Durable OAuth nonce store. State stays tenant-scoped from creation through
//! one-time consumption, so a process restart cannot weaken callback CSRF
//! protection or replay detection.

use async_trait::async_trait;
use sanvi_persistence::TxManager;

use crate::domain::{OAuthStateClaims, OAuthStateError, OAuthStateStore};

#[derive(Clone)]
pub struct SqlxOAuthStateStore {
    tx_manager: TxManager,
}

impl SqlxOAuthStateStore {
    pub fn new(tx_manager: TxManager) -> Self {
        Self { tx_manager }
    }
}

#[async_trait]
impl OAuthStateStore for SqlxOAuthStateStore {
    async fn issue(&self, claims: OAuthStateClaims) -> Result<(), OAuthStateError> {
        let mut scope = self
            .tx_manager
            .begin_for(claims.tenant_id.as_i64())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "advertising OAuth state insert scope failed");
                OAuthStateError::StoreUnavailable
            })?;
        let result = sqlx::query(
            "INSERT INTO advertising.oauth_states (nonce, tenant_id, platform, expires_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(claims.nonce)
        .bind(claims.tenant_id.as_i64())
        .bind(claims.platform.as_str())
        .bind(claims.expires_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await;
        match result {
            Ok(_) => scope.commit().await.map_err(|err| {
                tracing::error!(error = %err, "advertising OAuth state insert commit failed");
                OAuthStateError::StoreUnavailable
            }),
            Err(err) => {
                let _ = scope.rollback().await;
                tracing::error!(error = %err, "advertising OAuth state insert failed");
                Err(OAuthStateError::StoreUnavailable)
            }
        }
    }

    async fn consume(&self, claims: &OAuthStateClaims) -> Result<bool, OAuthStateError> {
        let mut scope = self
            .tx_manager
            .begin_for(claims.tenant_id.as_i64())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "advertising OAuth state consume scope failed");
                OAuthStateError::StoreUnavailable
            })?;
        let result = sqlx::query(
            "UPDATE advertising.oauth_states \
             SET redeemed_at = now() \
             WHERE nonce = $1 AND tenant_id = $2 AND platform = $3 \
               AND expires_at = $4 AND expires_at > now() AND redeemed_at IS NULL",
        )
        .bind(claims.nonce)
        .bind(claims.tenant_id.as_i64())
        .bind(claims.platform.as_str())
        .bind(claims.expires_at)
        .execute(&mut **scope.tx_mut().as_inner())
        .await;
        match result {
            Ok(result) => {
                let consumed = result.rows_affected() == 1;
                scope.commit().await.map_err(|err| {
                    tracing::error!(error = %err, "advertising OAuth state consume commit failed");
                    OAuthStateError::StoreUnavailable
                })?;
                Ok(consumed)
            }
            Err(err) => {
                let _ = scope.rollback().await;
                tracing::error!(error = %err, "advertising OAuth state consume failed");
                Err(OAuthStateError::StoreUnavailable)
            }
        }
    }
}
