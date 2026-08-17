//! Cross-context adapters: host → tenant resolution for the public theme
//! endpoint (over the HTTP platform's tenant resolver).

use std::sync::Arc;

use async_trait::async_trait;
use sanvi_http::{Resolution, TenantResolver};

use crate::domain::{ResolvedTenantRef, TenantContextResolver, ThemingError};

/// Adapts the HTTP platform's tenant resolver to the theming context's
/// port. An unresolved host yields `None` (the caller serves the platform
/// default theme); backend failures surface as errors so the public
/// endpoint can fail loudly instead of serving the wrong theme.
pub struct HostTenantResolverAdapter {
    inner: Arc<dyn TenantResolver>,
}

impl HostTenantResolverAdapter {
    pub fn new(inner: Arc<dyn TenantResolver>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl TenantContextResolver for HostTenantResolverAdapter {
    async fn resolve(&self, host: &str) -> Result<Option<ResolvedTenantRef>, ThemingError> {
        match self.inner.resolve(host, None).await {
            Ok(Resolution::Found(context)) => Ok(Some(ResolvedTenantRef {
                tenant_id: context.tenant_id,
                slug: context.slug,
            })),
            Ok(Resolution::NotFound) => Ok(None),
            Ok(Resolution::Rejected { detail }) => {
                tracing::debug!(%detail, "internal tenant header rejected on the public theme path");
                Ok(None)
            }
            Err(err) => Err(ThemingError::Storage(format!(
                "tenant resolution failed: {err}"
            ))),
        }
    }
}
