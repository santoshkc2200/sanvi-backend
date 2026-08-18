//! `POST /api/v1/tenant/domains/{id}/verify` — trigger a manual re-check.
//!
//! The worker owns the polling cadence; this endpoint only nudges it: the
//! domain moves to `verifying` with `next_check_at = now`, and the next
//! sweep picks it up. Throttled by the existing `next_check_at` schedule so
//! a frantic UI cannot hammer resolvers.

use std::sync::Arc;

use uuid::Uuid;

use sanvi_persistence::ScopedTx;

use crate::application::views::CustomDomainView;
use crate::domain::{Clock, CustomDomainRepository, DomainStatus, DomainsError};

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
        tenant_id: Uuid,
        domain_id: Uuid,
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
