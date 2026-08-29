use std::sync::Arc;

use sanvi_kernel::TenantId;

use crate::adapters::cache::LiveDomainCache;
use crate::domain::{CustomDomainRepository, DomainsError, Hostname};

pub struct LiveDomainLookupService {
    cache: Arc<LiveDomainCache>,
    domains: Arc<dyn CustomDomainRepository>,
    pool: sanvi_persistence::PgPool,
}

impl LiveDomainLookupService {
    pub fn new(
        cache: Arc<LiveDomainCache>,
        domains: Arc<dyn CustomDomainRepository>,
        pool: sanvi_persistence::PgPool,
    ) -> Self {
        Self {
            cache,
            domains,
            pool,
        }
    }

    /// The tenant a hostname resolves to, if it holds a live claim.
    pub async fn resolve(&self, host: &str) -> Option<TenantId> {
        let host = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
        let hostname = Hostname::parse(host.trim_end_matches('.')).ok()?;
        let normalized = hostname.as_str();

        if let Some(tenant) = self.cache.get(normalized).await {
            return Some(tenant);
        }

        let tenant = self
            .domains
            .find_live_by_hostname(&self.pool, normalized)
            .await
            .ok()
            .flatten()
            .map(|entry| entry.tenant_id);

        self.cache.put(normalized.to_string(), tenant).await;
        tenant
    }

    /// Drop the local + shared cache entries for a hostname (called after
    /// transitions that change resolution).
    pub async fn invalidate(&self, host: &str) -> Result<(), DomainsError> {
        self.cache.invalidate(host).await
    }

    /// The shared cache handle (the worker consumer invalidates it).
    pub fn cache(&self) -> Arc<crate::adapters::cache::LiveDomainCache> {
        self.cache.clone()
    }
}
