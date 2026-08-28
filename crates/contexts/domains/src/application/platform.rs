use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::application::views::CustomDomainView;
use crate::domain::{
    CertificateAuthorityPort, CertificateRepository, CustomDomainId, CustomDomainRepository,
    DnsProviderPort, DomainErrorCode, DomainKind, DomainsError, DomainsEvent, EventPublisher,
    storage,
};

pub struct PlatformListDomains {
    domains: Arc<dyn CustomDomainRepository>,
    tx: TxManager,
}

impl PlatformListDomains {
    pub fn new(domains: Arc<dyn CustomDomainRepository>, tx: TxManager) -> Self {
        Self { domains, tx }
    }

    pub async fn execute(
        &self,
        query: &str,
        limit: u32,
    ) -> Result<Vec<CustomDomainView>, DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.platform.search", "platform_admin")
            .await
            .map_err(storage)?;
        let domains = self
            .domains
            .platform_search(&mut scope, query.trim(), limit.min(100))
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(domains.iter().map(CustomDomainView::of).collect())
    }
}

/// Force-release: remove the hostname, revoke the certificate, delete the
/// zone. Audited through the platform scope's audit-log record.
pub struct PlatformReleaseDomain {
    domains: Arc<dyn CustomDomainRepository>,
    certs: Arc<dyn CertificateRepository>,
    provider: Arc<dyn DnsProviderPort>,
    ca: Arc<dyn CertificateAuthorityPort>,
    events: Arc<dyn EventPublisher>,
    tx: TxManager,
}

impl PlatformReleaseDomain {
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        certs: Arc<dyn CertificateRepository>,
        provider: Arc<dyn DnsProviderPort>,
        ca: Arc<dyn CertificateAuthorityPort>,
        events: Arc<dyn EventPublisher>,
        tx: TxManager,
    ) -> Self {
        Self {
            domains,
            certs,
            provider,
            ca,
            events,
            tx,
        }
    }

    pub async fn execute(
        &self,
        domain_id: CustomDomainId,
        reason: &str,
    ) -> Result<CustomDomainView, DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.platform.release", "platform_admin")
            .await
            .map_err(storage)?;

        let Some(mut domain) = self.domains.platform_find(&mut scope, domain_id).await? else {
            return Err(DomainsError::UnknownDomain(domain_id));
        };
        let now = chrono::Utc::now();
        domain.remove(
            DomainErrorCode::PLATFORM_RELEASE,
            format!("force-released by platform operations: {reason}"),
            now,
        )?;
        self.domains.update_platform(&mut scope, &domain).await?;
        self.domains
            .delete_live_host(&mut scope, domain.hostname.as_str())
            .await?;

        if let Some(cert) = self
            .certs
            .find_for_domain_platform(&mut scope, domain_id)
            .await?
        {
            self.ca.revoke(&cert).await?;
            let mut cert = cert;
            cert.revoke();
            self.certs.update_platform(&mut scope, &cert).await?;
        }

        if domain.kind == DomainKind::Purchased {
            // The zone rows are tenant-scoped; the platform scope bypasses
            // RLS for this audited maintenance path.
            self.provider
                .remove_zone_platform(&mut scope, domain_id)
                .await?;
        }

        self.events
            .publish(
                &mut scope,
                domain.tenant_id,
                &[DomainsEvent::DomainRemoved {
                    domain_id,
                    hostname: domain.hostname.as_str().to_string(),
                    reason: reason.to_string(),
                }],
            )
            .await?;

        scope.commit().await.map_err(storage)?;
        Ok(CustomDomainView::of(&domain))
    }
}
