//! Resolve a tenant by slug or id — the read path behind both the
//! resolution middleware and the public host-context endpoint.
//!
//! Cache-first with Postgres fallback: the cache is an optimisation, and
//! cache failure degrades with a warning, never to "no tenant".

use std::sync::Arc;

use sanvi_persistence::TxManager;
use tracing::warn;

use crate::domain::tenant::{Tenant, TenantId};
use crate::domain::values::TenantSlug;
use crate::domain::{TenancyError, TenantRepository, TenantResolutionCache};

use super::storage;

const ACTOR: &str = "tenant-resolution";

pub struct ResolveTenant {
    tx_manager: TxManager,
    tenants: Arc<dyn TenantRepository>,
    cache: Arc<dyn TenantResolutionCache>,
}

impl ResolveTenant {
    pub fn new(
        tx_manager: TxManager,
        tenants: Arc<dyn TenantRepository>,
        cache: Arc<dyn TenantResolutionCache>,
    ) -> Self {
        Self {
            tx_manager,
            tenants,
            cache,
        }
    }

    pub async fn by_slug(&self, slug: &TenantSlug) -> Result<Option<Tenant>, TenancyError> {
        match self.cache.get_slug(slug.as_str()).await {
            Ok(Some(tenant)) => return Ok(Some(tenant)),
            Ok(None) => {}
            Err(err) => {
                warn!(error = %err, slug = %slug, "resolution cache miss degraded to postgres")
            }
        }

        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_resolution", ACTOR)
            .await
            .map_err(storage)?;
        let tenant = self.tenants.find_by_slug(&mut scope, slug).await?;
        if let Some(tenant) = &tenant
            && let Err(err) = self.cache.put(tenant).await
        {
            warn!(error = %err, slug = %slug, "failed to warm resolution cache");
        }
        scope.commit().await.map_err(storage)?;
        Ok(tenant)
    }

    pub async fn by_id(&self, id: TenantId) -> Result<Option<Tenant>, TenancyError> {
        match self.cache.get_id(id.as_uuid()).await {
            Ok(Some(tenant)) => return Ok(Some(tenant)),
            Ok(None) => {}
            Err(err) => {
                warn!(error = %err, tenant_id = %id, "resolution cache miss degraded to postgres")
            }
        }

        let mut scope = self
            .tx_manager
            .begin_unscoped("tenant_resolution", ACTOR)
            .await
            .map_err(storage)?;
        let tenant = self.tenants.find_by_id(&mut scope, id).await?;
        if let Some(tenant) = &tenant
            && let Err(err) = self.cache.put(tenant).await
        {
            warn!(error = %err, tenant_id = %id, "failed to warm resolution cache");
        }
        scope.commit().await.map_err(storage)?;
        Ok(tenant)
    }

    /// Drop cached entries for a tenant (event-driven invalidation).
    pub async fn invalidate(&self, slug: &str, id: uuid::Uuid) {
        if let Err(err) = self.cache.invalidate(slug, id).await {
            warn!(error = %err, slug, tenant_id = %id, "failed to invalidate resolution cache");
        }
    }
}
