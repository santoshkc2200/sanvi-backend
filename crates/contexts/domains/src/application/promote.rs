//! `POST /api/v1/tenant/domains/{id}/promote` — make this domain primary.
//!
//! The previous primary (if any) becomes an alias; the partial unique
//! index `one_primary_per_tenant` guarantees the invariant at the storage
//! layer too.

use std::sync::Arc;

use uuid::Uuid;

use sanvi_persistence::ScopedTx;

use crate::application::views::CustomDomainView;
use crate::domain::{
    Clock, CustomDomainRepository, DomainRole, DomainsError, DomainsEvent, EventPublisher,
};

pub struct PromotePrimaryDomain {
    domains: Arc<dyn CustomDomainRepository>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl PromotePrimaryDomain {
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            domains,
            events,
            clock,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        domain_id: Uuid,
    ) -> Result<CustomDomainView, DomainsError> {
        let mut target = self
            .domains
            .find(scope, tenant_id, domain_id)
            .await?
            .ok_or(DomainsError::UnknownDomain(domain_id))?;

        let now = self.clock.now();
        if target.role == DomainRole::Primary {
            return Ok(CustomDomainView::of(&target));
        }

        // Demote the current primary first: the unique partial index only
        // admits one primary per tenant.
        if let Some(mut current) = self.domains.find_primary(scope, tenant_id).await? {
            current.demote_primary(now)?;
            self.domains.update(scope, &current).await?;
        }

        target.promote_primary(now)?;
        self.domains.update(scope, &target).await?;

        self.events
            .publish_scoped(
                scope,
                tenant_id,
                &[DomainsEvent::DomainClaimed {
                    domain_id: target.id,
                    hostname: target.hostname.as_str().to_string(),
                    domain_kind: target.kind.as_str().to_string(),
                    role: target.role.as_str().to_string(),
                }],
            )
            .await?;

        Ok(CustomDomainView::of(&target))
    }
}
