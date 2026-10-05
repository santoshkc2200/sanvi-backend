use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::ScopedTx;

use crate::application::views::{CustomDomainView, OrderView};
use crate::domain::{
    CertificateRepository, CertificateStatus, CustomDomainRepository, DomainOrderRepository,
    DomainsError,
};

pub struct ListCustomDomains {
    domains: Arc<dyn CustomDomainRepository>,
    certs: Arc<dyn CertificateRepository>,
}

impl ListCustomDomains {
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        certs: Arc<dyn CertificateRepository>,
    ) -> Self {
        Self { domains, certs }
    }

    #[tracing::instrument(name = "domains.list_custom_domains", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        limit: Option<u32>,
        after_id: Option<i64>,
    ) -> Result<Vec<CustomDomainView>, DomainsError> {
        let limit = sanvi_kernel::pagination::clamp_page_size(limit, 200, 200);
        let domains = self.domains.list(scope, tenant_id, limit, after_id).await?;
        let mut views = Vec::with_capacity(domains.len());
        for domain in &domains {
            let mut view = CustomDomainView::of(domain);
            let cert = self
                .certs
                .find_for_domain(scope, tenant_id, domain.id)
                .await?;
            view.cert_expires_at = cert
                .filter(|cert| cert.status == CertificateStatus::Active)
                .map(|cert| cert.not_after);
            views.push(view);
        }
        Ok(views)
    }
}

pub struct ListDomainOrders {
    orders: Arc<dyn DomainOrderRepository>,
}

impl ListDomainOrders {
    pub fn new(orders: Arc<dyn DomainOrderRepository>) -> Self {
        Self { orders }
    }

    #[tracing::instrument(name = "domains.list_domain_orders", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        limit: Option<u32>,
        after_id: Option<i64>,
    ) -> Result<Vec<OrderView>, DomainsError> {
        let limit = sanvi_kernel::pagination::clamp_page_size(limit, 200, 200);
        let orders = self.orders.list(scope, tenant_id, limit, after_id).await?;
        Ok(orders.iter().map(OrderView::of).collect())
    }
}
