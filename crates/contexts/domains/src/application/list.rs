//! Reads: the tenant's domains and orders.

use std::sync::Arc;

use uuid::Uuid;

use sanvi_persistence::ScopedTx;

use crate::application::views::{CustomDomainView, OrderView};
use crate::domain::{CustomDomainRepository, DomainOrderRepository, DomainsError};

pub struct ListCustomDomains {
    domains: Arc<dyn CustomDomainRepository>,
}

impl ListCustomDomains {
    pub fn new(domains: Arc<dyn CustomDomainRepository>) -> Self {
        Self { domains }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<CustomDomainView>, DomainsError> {
        let domains = self.domains.list(scope, tenant_id).await?;
        Ok(domains.iter().map(CustomDomainView::of).collect())
    }
}

pub struct ListDomainOrders {
    orders: Arc<dyn DomainOrderRepository>,
}

impl ListDomainOrders {
    pub fn new(orders: Arc<dyn DomainOrderRepository>) -> Self {
        Self { orders }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
    ) -> Result<Vec<OrderView>, DomainsError> {
        let orders = self.orders.list(scope, tenant_id).await?;
        Ok(orders.iter().map(OrderView::of).collect())
    }
}
