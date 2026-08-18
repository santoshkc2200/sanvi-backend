//! `GET /api/v1/tenant/domains/search?q=` — registrar search.
//!
//! Entitlement-gated (`domains.purchase`), cached with a short TTL, and
//! never fanning out uncontrolled registrar calls: one request, one
//! response list over the served TLD catalog.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::{DomainFeatureGate, DomainRegistrarPort, DomainsError, Hostname};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct DomainSearchView {
    pub query: String,
    pub registrar_id: String,
    pub results: Vec<crate::domain::DomainQuote>,
}

/// A tiny in-process TTL cache: search results are registrar-priced and
/// change slowly; a stampede of UI keystrokes must not become a stampede
/// of registrar calls.
pub struct SearchCache {
    entry: Mutex<Option<(String, DomainSearchView, tokio::time::Instant)>>,
}

impl SearchCache {
    pub fn new() -> Self {
        Self {
            entry: Mutex::new(None),
        }
    }

    pub async fn get(&self, query: &str, ttl: Duration) -> Option<DomainSearchView> {
        let guard = self.entry.lock().await;
        match guard.as_ref() {
            Some((cached_query, view, at)) if cached_query == query && at.elapsed() < ttl => {
                Some(view.clone())
            }
            _ => None,
        }
    }

    pub async fn put(&self, query: String, view: DomainSearchView) {
        *self.entry.lock().await = Some((query, view, tokio::time::Instant::now()));
    }
}

impl Default for SearchCache {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SearchDomains {
    registrar: Arc<dyn DomainRegistrarPort>,
    gate: Arc<dyn DomainFeatureGate>,
    cache: Arc<SearchCache>,
    purchase_entitlement_key: String,
    cache_ttl: Duration,
}

impl SearchDomains {
    pub fn new(
        registrar: Arc<dyn DomainRegistrarPort>,
        gate: Arc<dyn DomainFeatureGate>,
        cache: Arc<SearchCache>,
        purchase_entitlement_key: String,
        cache_ttl_secs: u64,
    ) -> Self {
        Self {
            registrar,
            gate,
            cache,
            purchase_entitlement_key,
            cache_ttl: Duration::from_secs(cache_ttl_secs),
        }
    }

    pub async fn execute(
        &self,
        tenant_id: Uuid,
        query: &str,
    ) -> Result<DomainSearchView, DomainsError> {
        if !self
            .gate
            .is_enabled(tenant_id, &self.purchase_entitlement_key)
            .await?
        {
            return Err(DomainsError::NotEntitled {
                feature: self.purchase_entitlement_key.clone(),
            });
        }

        let query = query.trim().trim_end_matches('.');
        if query.is_empty() || query.len() > 200 {
            return Err(DomainsError::InvalidHostname(format!(
                "invalid search query: {query}"
            )));
        }

        if let Some(view) = self.cache.get(query, self.cache_ttl).await {
            return Ok(view);
        }

        // A hostname-shaped query must still be valid; a bare term is fine.
        if query.contains('.') {
            Hostname::parse(query)?;
        }

        let results = self.registrar.search(query).await?;
        let view = DomainSearchView {
            query: query.to_string(),
            registrar_id: self.registrar.registrar_id(),
            results,
        };
        self.cache.put(query.to_string(), view.clone()).await;
        Ok(view)
    }
}
