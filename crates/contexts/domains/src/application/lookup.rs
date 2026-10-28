use std::sync::Arc;

use sanvi_kernel::TenantId;

use crate::adapters::cache::{CacheLookup, LiveDomainCache};
use crate::domain::{DomainsError, Hostname, LiveHostnameSource};

pub struct LiveDomainLookupService {
    cache: Arc<LiveDomainCache>,
    source: Arc<dyn LiveHostnameSource>,
}

impl LiveDomainLookupService {
    /// Construct lookup service with any live hostname source.
    pub fn new(cache: Arc<LiveDomainCache>, source: Arc<dyn LiveHostnameSource>) -> Self {
        Self { cache, source }
    }

    /// The tenant a hostname resolves to, if it holds a live claim.
    #[tracing::instrument(name = "domains.resolve", skip_all)]
    pub async fn resolve(&self, host: &str) -> Option<TenantId> {
        let host = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
        let hostname = Hostname::parse(host.trim_end_matches('.')).ok()?;
        let normalized = hostname.as_str();

        match self.cache.lookup(normalized).await {
            CacheLookup::Hit(tenant) => return tenant,
            CacheLookup::Miss => {}
        }

        match self.source.tenant_for(normalized).await {
            Ok(Some(tenant_id)) => {
                self.cache
                    .put(normalized.to_string(), Some(tenant_id))
                    .await;
                Some(tenant_id)
            }
            Ok(None) => {
                self.cache.put(normalized.to_string(), None).await;
                None
            }
            Err(err) => {
                tracing::warn!(
                    hostname = normalized,
                    error = %err,
                    "domain lookup failed; failing closed without caching"
                );
                None
            }
        }
    }

    /// Drop the local + shared cache entries for a hostname (called after
    /// transitions that change resolution).
    #[tracing::instrument(name = "domains.invalidate", skip_all)]
    pub async fn invalidate(&self, host: &str) -> Result<(), DomainsError> {
        self.cache.invalidate(host).await
    }

    /// The shared cache handle (the worker consumer invalidates it).
    pub fn cache(&self) -> Arc<LiveDomainCache> {
        self.cache.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    struct StubHostnameSource {
        tenant_id: Option<TenantId>,
        fail: bool,
        call_count: AtomicUsize,
    }

    #[async_trait]
    impl LiveHostnameSource for StubHostnameSource {
        async fn tenant_for(&self, _hostname: &str) -> Result<Option<TenantId>, DomainsError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(DomainsError::Storage("transport failure".into()));
            }
            Ok(self.tenant_id)
        }
    }

    #[tokio::test]
    async fn positive_resolution_is_cached() {
        let expected_tenant = TenantId::try_from(1001).unwrap();
        let source = Arc::new(StubHostnameSource {
            tenant_id: Some(expected_tenant),
            fail: false,
            call_count: AtomicUsize::new(0),
        });
        let cache = Arc::new(LiveDomainCache::new(None, 300));
        let service = LiveDomainLookupService::new(cache, source.clone());

        // First call calls source and caches positive result
        assert_eq!(
            service.resolve("shop.example.com").await,
            Some(expected_tenant)
        );
        assert_eq!(source.call_count.load(Ordering::SeqCst), 1);

        // Second call hits cache; source is not called again
        assert_eq!(
            service.resolve("shop.example.com").await,
            Some(expected_tenant)
        );
        assert_eq!(source.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn negative_resolution_is_cached() {
        let source = Arc::new(StubHostnameSource {
            tenant_id: None,
            fail: false,
            call_count: AtomicUsize::new(0),
        });
        let cache = Arc::new(LiveDomainCache::new(None, 300));
        let service = LiveDomainLookupService::new(cache, source.clone());

        // First call calls source and caches negative result (None)
        assert_eq!(service.resolve("unknown.example.com").await, None);
        assert_eq!(source.call_count.load(Ordering::SeqCst), 1);

        // Second call hits negative cache; source is not called again
        assert_eq!(service.resolve("unknown.example.com").await, None);
        assert_eq!(source.call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn transport_error_fails_closed_without_caching() {
        let source = Arc::new(StubHostnameSource {
            tenant_id: None,
            fail: true,
            call_count: AtomicUsize::new(0),
        });
        let cache = Arc::new(LiveDomainCache::new(None, 300));
        let service = LiveDomainLookupService::new(cache, source.clone());

        // Transport error fails closed (returns None)
        assert_eq!(service.resolve("error.example.com").await, None);
        assert_eq!(source.call_count.load(Ordering::SeqCst), 1);

        // Because it was an error, it is NOT cached: subsequent call retries source
        assert_eq!(service.resolve("error.example.com").await, None);
        assert_eq!(source.call_count.load(Ordering::SeqCst), 2);
    }
}
