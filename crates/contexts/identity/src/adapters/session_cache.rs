//! The verified-session cache: Redis, keyed by session-token hash, TTL ≤ 60s,
//! invalidated by session id on the logout webhook.
//!
//! Two keys per session: `sanvi:session:{token_hash}` (the cached session
//! info, TTL) and `sanvi:session:idx:{session_id}` (token_hash index, no
//! TTL). Logout resolves the index and deletes both, so a revoked session
//! can never stay cached until TTL.

use async_trait::async_trait;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use crate::domain::IdentityError;
use crate::domain::ports::{SessionCache, SessionInfo};

const KEY_PREFIX: &str = "sanvi:session";
const INDEX_PREFIX: &str = "sanvi:session:idx";

#[derive(Clone)]
pub struct RedisSessionCache {
    redis: ConnectionManager,
}

impl RedisSessionCache {
    pub fn new(redis: ConnectionManager) -> Self {
        Self { redis }
    }
}

#[async_trait]
impl SessionCache for RedisSessionCache {
    async fn get(&self, token_hash: &str) -> Result<Option<SessionInfo>, IdentityError> {
        let key = format!("{KEY_PREFIX}:{token_hash}");
        let mut redis = self.redis.clone();
        let raw: Option<String> = redis
            .get(&key)
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        match raw {
            None => Ok(None),
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|err| IdentityError::Storage(err.to_string())),
        }
    }

    async fn put(
        &self,
        token_hash: &str,
        session: &SessionInfo,
        ttl_secs: u64,
    ) -> Result<(), IdentityError> {
        let json = serde_json::to_string(session)
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        let key = format!("{KEY_PREFIX}:{token_hash}");
        let index_key = format!("{INDEX_PREFIX}:{}", session.session_id);
        let mut redis = self.redis.clone();
        let _: () = redis
            .set_ex(&key, &json, ttl_secs)
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        // The index lives slightly longer than the session entry so a
        // late-arriving logout webhook can still invalidate.
        let _: () = redis
            .set_ex(&index_key, token_hash, ttl_secs + 60)
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn invalidate_session(&self, session_id: &str) -> Result<(), IdentityError> {
        let index_key = format!("{INDEX_PREFIX}:{session_id}");
        let mut redis = self.redis.clone();
        let token_hash: Option<String> = redis
            .get(&index_key)
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        if let Some(token_hash) = token_hash {
            let key = format!("{KEY_PREFIX}:{token_hash}");
            let _: () = redis
                .del(&key)
                .await
                .map_err(|err| IdentityError::Storage(err.to_string()))?;
        }
        let _: () = redis
            .del(&index_key)
            .await
            .map_err(|err| IdentityError::Storage(err.to_string()))?;
        Ok(())
    }
}

/// A no-op cache for environments without Redis: every verify goes to
/// Kratos (correct, just slower).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSessionCache;

#[async_trait]
impl SessionCache for NoopSessionCache {
    async fn get(&self, _token_hash: &str) -> Result<Option<SessionInfo>, IdentityError> {
        Ok(None)
    }

    async fn put(
        &self,
        _token_hash: &str,
        _session: &SessionInfo,
        _ttl_secs: u64,
    ) -> Result<(), IdentityError> {
        Ok(())
    }

    async fn invalidate_session(&self, _session_id: &str) -> Result<(), IdentityError> {
        Ok(())
    }
}
