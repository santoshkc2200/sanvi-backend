//! Cross-context adapter: host → tenant resolution for the public shopper
//! scope, over the HTTP platform's tenant resolver (the same pattern the
//! theming context's public theme endpoint uses).

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_http::{Resolution, TenantResolver};

use crate::domain::{HostTenants, ProductAccessError, ShopperTenant};

/// Adapts the platform's tenant resolver to the shopper path's port. An
/// unresolved host yields `None` (the caller renders store-unavailable);
/// a resolved tenant that is not active still resolves, with
/// `active: false`, so the caller renders the same honest state instead
/// of leaking lifecycle detail.
pub struct HostTenantShopperAdapter {
    inner: Arc<dyn TenantResolver>,
}

impl HostTenantShopperAdapter {
    pub fn new(inner: Arc<dyn TenantResolver>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl HostTenants for HostTenantShopperAdapter {
    async fn resolve(&self, host: &str) -> Result<Option<ShopperTenant>, ProductAccessError> {
        match self.inner.resolve(host, None).await {
            Ok(Resolution::Found(context)) => Ok(Some(ShopperTenant {
                tenant_id: context.tenant_id,
                active: matches!(
                    context.status,
                    sanvi_http::tenancy::TenantRuntimeStatus::Active
                ),
            })),
            Ok(Resolution::NotFound) => Ok(None),
            // The public scope never carries the internal header, so a
            // rejection cannot happen; treat it as unresolved.
            Ok(Resolution::Rejected { detail }) => {
                tracing::debug!(%detail, "tenant resolution rejected on the shopper path");
                Ok(None)
            }
            Err(err) => Err(ProductAccessError::Storage(format!(
                "tenant resolution failed: {err}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolver stub building a fixed resolution per call.
    struct Stub(Kind);

    enum Kind {
        Found(sanvi_http::tenancy::TenantRuntimeStatus),
        NotFound,
        Rejected,
    }

    impl TenantResolver for Stub {
        fn resolve<'a>(
            &'a self,
            _: &'a str,
            _: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Resolution, sanvi_http::ApiError>>
                    + Send
                    + 'a,
            >,
        > {
            let kind = match self.0 {
                Kind::Found(status) => Resolution::Found(context(status)),
                Kind::NotFound => Resolution::NotFound,
                Kind::Rejected => Resolution::Rejected {
                    detail: "no header channel".to_owned(),
                },
            };
            Box::pin(async { Ok(kind) })
        }
    }

    fn context(
        status: sanvi_http::tenancy::TenantRuntimeStatus,
    ) -> sanvi_http::tenancy::TenantContext {
        sanvi_http::tenancy::TenantContext {
            tenant_id: sanvi_kernel::TenantId::try_from(1001_i64).unwrap(),
            slug: "acme".to_owned(),
            display_name: "Acme".to_owned(),
            status,
            region: "eu".to_owned(),
            default_locale: "en".to_owned(),
            resolution_source: sanvi_http::tenancy::ResolutionSource::CustomDomain,
        }
    }

    #[tokio::test]
    async fn active_and_inactive_tenants_resolve_with_their_liveness() {
        let adapter = HostTenantShopperAdapter::new(Arc::new(Stub(Kind::Found(
            sanvi_http::tenancy::TenantRuntimeStatus::Active,
        ))));
        let resolved = adapter.resolve("shop.acme.test").await.unwrap().unwrap();
        assert_eq!(resolved.tenant_id.as_i64(), 1001);
        assert!(resolved.active);

        let suspended = HostTenantShopperAdapter::new(Arc::new(Stub(Kind::Found(
            sanvi_http::tenancy::TenantRuntimeStatus::Suspended,
        ))));
        let resolved = suspended.resolve("shop.acme.test").await.unwrap().unwrap();
        assert!(
            !resolved.active,
            "a suspended tenant must not count as live"
        );
    }

    #[tokio::test]
    async fn unknown_hosts_and_rejections_are_unresolved() {
        let none = HostTenantShopperAdapter::new(Arc::new(Stub(Kind::NotFound)));
        assert!(none.resolve("nobody.test").await.unwrap().is_none());
        let rejected = HostTenantShopperAdapter::new(Arc::new(Stub(Kind::Rejected)));
        assert!(rejected.resolve("any.test").await.unwrap().is_none());
    }
}
