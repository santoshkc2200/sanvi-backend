//! The certificate issuance sweep (worker job): verified domains get their
//! first certificate. The reference sandbox CA issues deterministically
//! (DNS-01 semantics for purchased domains, HTTP-01 for connected ones —
//! both reduced to "verified ⇒ issuable" by the authorize gate); a
//! production adapter drives the edge's ACME pipeline through the same
//! port.

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::domain::{
    CertificateAuthorityPort, CertificateRepository, Clock, CustomDomainRepository, DomainStatus,
    DomainsError, DomainsEvent, EventPublisher, storage,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CertIssuanceReport {
    pub issued: u32,
    pub failures: u32,
}

pub struct RunCertIssuanceSweep {
    domains: Arc<dyn CustomDomainRepository>,
    certs: Arc<dyn CertificateRepository>,
    ca: Arc<dyn CertificateAuthorityPort>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    tx: TxManager,
}

impl RunCertIssuanceSweep {
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        certs: Arc<dyn CertificateRepository>,
        ca: Arc<dyn CertificateAuthorityPort>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        tx: TxManager,
    ) -> Self {
        Self {
            domains,
            certs,
            ca,
            events,
            clock,
            tx,
        }
    }

    #[tracing::instrument(name = "domains.run_cert_issuance_sweep", skip_all)]
    pub async fn execute(&self, limit: u32) -> Result<CertIssuanceReport, DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.cert_issuance_sweep", "domains-worker")
            .await
            .map_err(storage)?;
        let awaiting = self.domains.awaiting_cert(&mut scope, limit).await?;
        scope.commit().await.map_err(storage)?;

        let mut report = CertIssuanceReport::default();
        for mut domain in awaiting {
            let now = self.clock.now();
            // Move to issuing_cert first so a crash mid-issuance leaves an
            // unambiguous state to retry from. Rows already in
            // `issuing_cert` (a previous failure or crash) are picked up
            // again here without a transition.
            if domain.status == DomainStatus::Verified {
                domain.start_cert_issuance(now)?;
                self.persist_domain(&domain).await?;
            }

            match self.ca.issue(&domain).await {
                Ok(cert) => {
                    let mut scope = self
                        .tx
                        .begin_unscoped("domains.cert_issuance_sweep.issue", "domains-worker")
                        .await
                        .map_err(storage)?;
                    self.certs.insert_platform(&mut scope, &cert).await?;
                    domain.mark_live(self.clock.now())?;
                    self.domains.update_platform(&mut scope, &domain).await?;
                    self.domains
                        .upsert_live_host(
                            &mut scope,
                            domain.tenant_id,
                            domain.hostname.as_str(),
                            domain.status.as_str(),
                        )
                        .await?;
                    self.events
                        .publish(
                            &mut scope,
                            domain.tenant_id,
                            &[
                                DomainsEvent::CertificateIssued {
                                    domain_id: domain.id,
                                    hostname: domain.hostname.as_str().to_string(),
                                    not_after: cert.not_after,
                                },
                                DomainsEvent::DomainWentLive {
                                    domain_id: domain.id,
                                    hostname: domain.hostname.as_str().to_string(),
                                },
                            ],
                        )
                        .await?;
                    scope.commit().await.map_err(storage)?;
                    report.issued += 1;
                }
                Err(err) => {
                    tracing::warn!(
                        hostname = domain.hostname.as_str(),
                        error = %err,
                        "certificate issuance failed; the domain stays in issuing_cert and is retried on the next sweep"
                    );
                    domain.failure = Some(crate::domain::DomainFailure {
                        code: "cert_issuance_failed".to_string(),
                        detail: err.to_string(),
                        ..Default::default()
                    });
                    self.persist_domain(&domain).await?;
                    report.failures += 1;
                }
            }
        }
        Ok(report)
    }

    async fn persist_domain(
        &self,
        domain: &crate::domain::CustomDomain,
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.cert_issuance_sweep.persist", "domains-worker")
            .await
            .map_err(storage)?;
        self.domains.update_platform(&mut scope, domain).await?;
        scope.commit().await.map_err(storage)
    }
}
