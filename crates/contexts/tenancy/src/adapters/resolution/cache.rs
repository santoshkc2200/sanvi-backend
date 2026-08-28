//! Redis-backed resolution cache: host→tenant lookups with a short TTL,
//! invalidated on tenant lifecycle events by the worker's consumer.

use std::time::Duration;

use crate::domain::tenant::{Tenant, TenantId};
use crate::domain::{TenancyError, TenantResolutionCache};
use async_trait::async_trait;
use redis::AsyncCommands;

/// Key namespace for resolution entries (platform-level: no tenant exists
/// *before* resolution, so the tenant-scoped `TenantCache` shape does not
/// apply).
fn slug_key(slug: &str) -> String {
    format!("sanvi:tenant:resolution:slug:{slug}")
}

fn id_key(id: TenantId) -> String {
    format!("sanvi:tenant:resolution:id:{id}")
}

#[derive(Clone)]
pub struct RedisTenantResolutionCache {
    conn: redis::aio::ConnectionManager,
    ttl: Duration,
}

impl RedisTenantResolutionCache {
    pub fn new(conn: redis::aio::ConnectionManager, ttl: Duration) -> Self {
        Self { conn, ttl }
    }

    async fn get_raw(&self, key: &str) -> Result<Option<Tenant>, TenancyError> {
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn
            .get(key)
            .await
            .map_err(|err| TenancyError::Storage(err.to_string()))?;
        match raw {
            None => Ok(None),
            Some(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|err| TenancyError::Storage(err.to_string())),
        }
    }

    async fn put_raw(&self, key: &str, tenant: &Tenant) -> Result<(), TenancyError> {
        let json =
            serde_json::to_string(tenant).map_err(|err| TenancyError::Storage(err.to_string()))?;
        let mut conn = self.conn.clone();
        let _: () = conn
            .set_ex(key, json, self.ttl.as_secs())
            .await
            .map_err(|err| TenancyError::Storage(err.to_string()))?;
        Ok(())
    }

    async fn invalidate_raw(&self, key: &str) -> Result<(), TenancyError> {
        let mut conn = self.conn.clone();
        let _: () = conn
            .del(key)
            .await
            .map_err(|err| TenancyError::Storage(err.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl TenantResolutionCache for RedisTenantResolutionCache {
    async fn get_slug(&self, slug: &str) -> Result<Option<Tenant>, TenancyError> {
        self.get_raw(&slug_key(slug)).await
    }

    async fn get_id(&self, id: TenantId) -> Result<Option<Tenant>, TenancyError> {
        self.get_raw(&id_key(id)).await
    }

    async fn put(&self, tenant: &Tenant) -> Result<(), TenancyError> {
        self.put_raw(&slug_key(tenant.slug().as_str()), tenant)
            .await?;
        self.put_raw(&id_key(tenant.id()), tenant).await
    }

    async fn invalidate(&self, slug: &str, id: TenantId) -> Result<(), TenancyError> {
        self.invalidate_raw(&slug_key(slug)).await?;
        self.invalidate_raw(&id_key(id)).await
    }
}

/// Cache disabled (no Redis configured): every lookup degrades to Postgres.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTenantResolutionCache;

#[async_trait]
impl TenantResolutionCache for NoopTenantResolutionCache {
    async fn get_slug(&self, _slug: &str) -> Result<Option<Tenant>, TenancyError> {
        Ok(None)
    }

    async fn get_id(&self, _id: TenantId) -> Result<Option<Tenant>, TenancyError> {
        Ok(None)
    }

    async fn put(&self, _tenant: &Tenant) -> Result<(), TenancyError> {
        Ok(())
    }

    async fn invalidate(&self, _slug: &str, _id: TenantId) -> Result<(), TenancyError> {
        Ok(())
    }
}
