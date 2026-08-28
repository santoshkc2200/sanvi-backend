use std::sync::Arc;

use sanvi_kernel::TenantId;
use sanvi_persistence::ScopedTx;

use crate::application::views::CustomDomainView;
use crate::domain::{
    CertificateAuthorityPort, CertificateRepository, Clock, CustomDomainId, CustomDomainRepository,
    DnsProviderPort, DomainErrorCode, DomainKind, DomainsError, DomainsEvent, EventPublisher,
};

pub struct RemoveCustomDomain {
    domains: Arc<dyn CustomDomainRepository>,
    certs: Arc<dyn CertificateRepository>,
    provider: Arc<dyn DnsProviderPort>,
    ca: Arc<dyn CertificateAuthorityPort>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
}

impl RemoveCustomDomain {
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        certs: Arc<dyn CertificateRepository>,
        provider: Arc<dyn DnsProviderPort>,
        ca: Arc<dyn CertificateAuthorityPort>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            domains,
            certs,
            provider,
            ca,
            events,
            clock,
        }
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

        let now = self.clock.now();
        domain.remove(
            DomainErrorCode::REMOVED_BY_TENANT,
            "removed by the tenant".to_string(),
            now,
        )?;
        self.domains.update(scope, &domain).await?;
        // Exit from the live set: the hostname must stop resolving.
        self.domains
            .delete_live_host_scoped(scope, domain.hostname.as_str())
            .await?;

        if let Some(cert) = self
            .certs
            .find_for_domain(scope, tenant_id, domain_id)
            .await?
        {
            self.ca.revoke(&cert).await?;
            let mut cert = cert;
            cert.revoke();
            self.certs.update(scope, &cert).await?;
        }

        if domain.kind == DomainKind::Purchased {
            self.provider.remove_zone(scope, domain_id).await?;
        }

        self.events
            .publish_scoped(
                scope,
                tenant_id,
                &[DomainsEvent::DomainRemoved {
                    domain_id,
                    hostname: domain.hostname.as_str().to_string(),
                    reason: DomainErrorCode::REMOVED_BY_TENANT.to_string(),
                }],
            )
            .await?;

        Ok(CustomDomainView::of(&domain))
    }
}
