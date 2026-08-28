use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::ScopedTx;

use crate::application::views::CustomDomainView;
use crate::domain::{
    Clock, CustomDomainId, CustomDomainRepository, DomainStatus, DomainsError,
};

pub struct RequestVerification {
    domains: Arc<dyn CustomDomainRepository>,
    clock: Arc<dyn Clock>,
}

impl RequestVerification {
    pub fn new(domains: Arc<dyn CustomDomainRepository>, clock: Arc<dyn Clock>) -> Self {
        Self { domains, clock }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        domain_id: CustomDomainId,
    ) -> Result<CustomDomainView, DomainsError> {
        let mut domain = self
            .domains
            .find(scope, tenant_id, domain_id)
            .await?
            .ok_or(DomainsError::UnknownDomain(domain_id))?;

        if domain.status == DomainStatus::Removed {
            return Err(DomainsError::DomainRemoved);
        }

        let now = self.clock.now();
        if let Some(next) = domain.next_check_at
            && next > now
            && domain.status == DomainStatus::Verifying
        {
            return Err(DomainsError::VerificationThrottled { next });
        }

        domain.start_verification(now)?;
        self.domains.update(scope, &domain).await?;
        Ok(CustomDomainView::of(&domain))
    }
}
