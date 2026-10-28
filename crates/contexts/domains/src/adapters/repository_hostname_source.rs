//! Repository-backed live hostname resolution source.

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_kernel::TenantId;

use crate::domain::{CustomDomainRepository, DomainsError, LiveHostnameSource};

/// Resolves live hostnames against the local database via `CustomDomainRepository`.
#[derive(Clone)]
pub struct RepositoryHostnameSource {
    domains: Arc<dyn CustomDomainRepository>,
    pool: sanvi_persistence::PgPool,
}

impl RepositoryHostnameSource {
    pub fn new(domains: Arc<dyn CustomDomainRepository>, pool: sanvi_persistence::PgPool) -> Self {
        Self { domains, pool }
    }
}

#[async_trait]
impl LiveHostnameSource for RepositoryHostnameSource {
    async fn tenant_for(&self, hostname: &str) -> Result<Option<TenantId>, DomainsError> {
        let entry = self
            .domains
            .find_live_by_hostname(&self.pool, hostname)
            .await?;
        Ok(entry.map(|e| e.tenant_id))
    }
}
