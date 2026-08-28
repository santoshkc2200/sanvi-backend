//! Host-based tenant resolution: the implementation behind the HTTP
//! middleware's `TenantResolver` port.
//!
//! Order (most specific wins):
//! 1. Signed `x-sanvi-tenant` header (service-to-service, worker).
//! 2. Session/JWT tenant claim (phase 02 — slot reserved).
//! 3. Custom-domain lookup hook (phase 08 — slot reserved).
//! 4. `<slug>.<base-domain>` subdomain of the Host header.

mod cache;
mod signed;

use std::pin::Pin;
use std::sync::Arc;

use sanvi_http::{
    ApiError, Resolution, ResolutionSource, TenantContext, TenantResolver, TenantRuntimeStatus,
};

pub use cache::{NoopTenantResolutionCache, RedisTenantResolutionCache};
pub use signed::{SignedTokenError, SigningKey, mint, verify};

use crate::application::ResolveTenant;
use crate::domain::TenancyError;
use crate::domain::tenant::{Tenant, TenantId};
use crate::domain::values::TenantSlug;

/// Phase-08 slot: resolves custom domains to tenant ids. Empty until the
/// `domains` context lands.
#[async_trait::async_trait]
pub trait CustomDomainLookup: Send + Sync {
    async fn lookup(&self, host: &str) -> Result<Option<TenantId>, TenancyError>;
}

/// Convert an aggregate into the middleware's context shape.
pub fn tenant_context_from(tenant: &Tenant, source: ResolutionSource) -> TenantContext {
    TenantContext {
        tenant_id: tenant.id().as_i64(),
        slug: tenant.slug().to_string(),
        display_name: tenant.display_name().to_string(),
        status: match tenant.status() {
            crate::domain::TenantStatus::Provisioning => TenantRuntimeStatus::Provisioning,
            crate::domain::TenantStatus::Active => TenantRuntimeStatus::Active,
            crate::domain::TenantStatus::Suspended => TenantRuntimeStatus::Suspended,
            crate::domain::TenantStatus::Archived => TenantRuntimeStatus::Archived,
            crate::domain::TenantStatus::Purged => TenantRuntimeStatus::Archived,
        },
        region: tenant.region().to_string(),
        default_locale: tenant.default_locale().to_string(),
        resolution_source: source,
    }
}

/// The resolver wired into the middleware stack.
pub struct HostTenantResolver {
    resolve: Arc<ResolveTenant>,
    base_domain: String,
    keys: Vec<SigningKey>,
    custom_domains: Option<Arc<dyn CustomDomainLookup>>,
}

impl HostTenantResolver {
    pub fn new(
        resolve: Arc<ResolveTenant>,
        base_domain: impl Into<String>,
        keys: Vec<SigningKey>,
    ) -> Self {
        Self {
            resolve,
            base_domain: base_domain.into(),
            keys,
            custom_domains: None,
        }
    }

    /// Phase-08 slot: install the custom-domain lookup hook.
    pub fn with_custom_domains(mut self, lookup: Arc<dyn CustomDomainLookup>) -> Self {
        self.custom_domains = Some(lookup);
        self
    }

    pub fn base_domain(&self) -> &str {
        &self.base_domain
    }

    /// Extract `<slug>` from a Host header ending in `.{base_domain}`.
    /// Ports are stripped (`acme.localhost:8080`); matching is
    /// case-insensitive (DNS is). Hosts that do not match the base domain
    /// (or match it exactly) yield `None`.
    pub fn subdomain_slug<'a>(&self, host: &'a str) -> Option<&'a str> {
        let host = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
        let host = host.trim_end_matches('.');
        let lower = host.to_ascii_lowercase();
        let suffix = format!(".{}", self.base_domain.to_ascii_lowercase());
        let prefix = lower.strip_suffix(&suffix)?;
        if prefix.is_empty() || prefix.contains('.') {
            return None;
        }
        // Slice the original so the case the caller sent is preserved
        // (slug parsing lowercases anyway).
        Some(&host[..prefix.len()])
    }

    async fn resolve_slug(&self, slug: &str) -> Result<Resolution, ApiError> {
        let slug = match TenantSlug::parse(slug) {
            Ok(slug) => slug,
            // Hostile/homoglyph hosts: not a tenant (and never an error
            // that leaks anything).
            Err(_) => return Ok(Resolution::NotFound),
        };
        let tenant = self
            .resolve
            .by_slug(&slug)
            .await
            .map_err(|err| ApiError::service_unavailable(err.to_string()))?;
        Ok(match tenant {
            Some(tenant) => {
                Resolution::Found(tenant_context_from(&tenant, ResolutionSource::Subdomain))
            }
            None => Resolution::NotFound,
        })
    }

    /// Host-based resolution for the public `GET /tenant-context` endpoint
    /// (storefront SSR bootstrap): no internal header, just subdomain (and
    /// the phase-08 custom-domain hook).
    pub async fn resolve_public(&self, host: &str) -> Result<Option<TenantContext>, ApiError> {
        if let Some(lookup) = &self.custom_domains
            && let Ok(Some(tenant_id)) = lookup.lookup(host).await
        {
            let tenant = self
                .resolve
                .by_id(tenant_id)
                .await
                .map_err(|err| ApiError::service_unavailable(err.to_string()))?;
            return Ok(
                tenant.map(|tenant| tenant_context_from(&tenant, ResolutionSource::CustomDomain))
            );
        }
        match self.subdomain_slug(host) {
            Some(slug) => {
                let slug = TenantSlug::parse(slug).ok();
                match slug {
                    Some(slug) => Ok(self
                        .resolve
                        .by_slug(&slug)
                        .await
                        .map_err(|err| ApiError::service_unavailable(err.to_string()))?
                        .map(|tenant| tenant_context_from(&tenant, ResolutionSource::Subdomain))),
                    None => Ok(None),
                }
            }
            None => Ok(None),
        }
    }
}

impl TenantResolver for HostTenantResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        internal_header: Option<&'a str>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Resolution, ApiError>> + Send + 'a>> {
        Box::pin(async move {
            // 1. Signed internal header: most specific source wins.
            if let Some(token) = internal_header {
                if self.keys.is_empty() {
                    // Header present but the channel is not configured:
                    // loud rejection, never a silent fall-through.
                    return Ok(Resolution::Rejected {
                        detail: "internal tenant header is not configured".to_string(),
                    });
                }
                let tenant_id = match verify(
                    token,
                    &self.keys,
                    chrono::Utc::now(),
                    sanvi_http::tenancy::TENANT_HEADER_TTL,
                ) {
                    Ok(id) => id,
                    Err(err) => {
                        return Ok(Resolution::Rejected {
                            detail: err.to_string(),
                        });
                    }
                };
                let tenant = self
                    .resolve
                    .by_id(tenant_id)
                    .await
                    .map_err(|err| ApiError::service_unavailable(err.to_string()))?;
                return Ok(match tenant {
                    Some(tenant) => Resolution::Found(tenant_context_from(
                        &tenant,
                        ResolutionSource::InternalHeader,
                    )),
                    None => Resolution::NotFound,
                });
            }

            // 2. Session/JWT tenant claim — phase 02 slot.

            // 3. Custom-domain lookup hook — phase 08 slot.
            if let Some(lookup) = &self.custom_domains
                && let Ok(Some(tenant_id)) = lookup.lookup(host).await
            {
                let tenant = self
                    .resolve
                    .by_id(tenant_id)
                    .await
                    .map_err(|err| ApiError::service_unavailable(err.to_string()))?;
                return Ok(match tenant {
                    Some(tenant) => Resolution::Found(tenant_context_from(
                        &tenant,
                        ResolutionSource::CustomDomain,
                    )),
                    None => Resolution::NotFound,
                });
            }

            // 4. Subdomain.
            match self.subdomain_slug(host) {
                Some(slug) => self.resolve_slug(slug).await,
                None => Ok(Resolution::NotFound),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(base_domain: &str) -> HostTenantResolver {
        // Never wired to storage: subdomain extraction tests never resolve.
        let resolve = Arc::new(ResolveTenant::new(
            sanvi_persistence::TxManager::new(
                sanvi_persistence::PgPool::connect_lazy(
                    "postgres://nobody:nothing@127.0.0.1:1/none",
                )
                .expect("lazy pool construction never connects"),
                sanvi_testsupport::test_ids(),
            ),
            Arc::new(UnreachableRepo),
            Arc::new(UnreachableCache),
        ));
        HostTenantResolver::new(resolve, base_domain.to_string(), Vec::new())
    }

    struct UnreachableRepo;
    #[async_trait::async_trait]
    impl crate::domain::TenantRepository for UnreachableRepo {
        async fn save(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _tenant: &Tenant,
        ) -> Result<(), TenancyError> {
            unreachable!()
        }
        async fn find_by_id(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _id: crate::domain::TenantId,
        ) -> Result<Option<Tenant>, TenancyError> {
            unreachable!()
        }
        async fn find_by_slug(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _slug: &TenantSlug,
        ) -> Result<Option<Tenant>, TenancyError> {
            unreachable!()
        }
        async fn list(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _after: Option<crate::domain::TenantId>,
            _limit: u32,
            _status: Option<crate::domain::TenantStatus>,
        ) -> Result<(Vec<Tenant>, Option<crate::domain::TenantId>), TenancyError> {
            unreachable!()
        }
        async fn due_for_purge_request(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _before: chrono::DateTime<chrono::Utc>,
            _limit: u32,
        ) -> Result<Vec<Tenant>, TenancyError> {
            unreachable!()
        }
        async fn search(
            &self,
            _scope: &mut sanvi_persistence::PlatformScope<'_>,
            _search: &crate::domain::TenantSearch,
        ) -> Result<(Vec<Tenant>, Option<crate::domain::TenantSearchCursor>), TenancyError>
        {
            unreachable!()
        }
    }

    struct UnreachableCache;
    #[async_trait::async_trait]
    impl crate::domain::TenantResolutionCache for UnreachableCache {
        async fn get_slug(&self, _slug: &str) -> Result<Option<Tenant>, TenancyError> {
            unreachable!()
        }
        async fn get_id(&self, _id: TenantId) -> Result<Option<Tenant>, TenancyError> {
            unreachable!()
        }
        async fn put(&self, _tenant: &Tenant) -> Result<(), TenancyError> {
            unreachable!()
        }
        async fn invalidate(&self, _slug: &str, _id: TenantId) -> Result<(), TenancyError> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn subdomain_extraction_strips_ports_and_matches_the_base() {
        let resolver = resolver("platform.sanvi.app");
        assert_eq!(
            resolver.subdomain_slug("acme.platform.sanvi.app"),
            Some("acme")
        );
        assert_eq!(
            resolver.subdomain_slug("acme.platform.sanvi.app:8080"),
            Some("acme")
        );
        assert_eq!(
            resolver.subdomain_slug("Acme.Platform.Sanvi.App:8080"),
            Some("Acme")
        );
        assert_eq!(resolver.subdomain_slug("platform.sanvi.app"), None);
        assert_eq!(resolver.subdomain_slug("a.b.platform.sanvi.app"), None);
        assert_eq!(resolver.subdomain_slug("evil.example.com"), None);
        assert_eq!(resolver.subdomain_slug("platform.sanvi.app.evil.com"), None);
    }

    #[tokio::test]
    async fn subdomain_extraction_handles_localhost_dev() {
        let resolver = resolver("localhost");
        assert_eq!(resolver.subdomain_slug("acme.localhost"), Some("acme"));
        assert_eq!(resolver.subdomain_slug("acme.localhost:8080"), Some("acme"));
        assert_eq!(resolver.subdomain_slug("localhost"), None);
    }

    #[tokio::test]
    async fn trailing_dot_is_tolerated() {
        let resolver = resolver("platform.sanvi.app");
        assert_eq!(
            resolver.subdomain_slug("acme.platform.sanvi.app."),
            Some("acme")
        );
    }

    #[test]
    fn context_mapping_reflects_runtime_statuses() {
        let (tenant, _) = Tenant::provision(
            TenantId::try_from(101_i64).unwrap(),
            TenantSlug::parse("acme").unwrap(),
            crate::domain::TenantName::parse("Acme").unwrap(),
            crate::domain::DataRegion::Jp,
            crate::domain::Locale::parse("ja").unwrap(),
            chrono::Utc::now(),
        );
        let context = tenant_context_from(&tenant, ResolutionSource::Subdomain);
        assert_eq!(context.slug, "acme");
        assert_eq!(context.region, "jp");
        assert_eq!(context.default_locale, "ja");
        assert_eq!(context.status, TenantRuntimeStatus::Provisioning);
        assert_eq!(context.resolution_source, ResolutionSource::Subdomain);
    }
}
