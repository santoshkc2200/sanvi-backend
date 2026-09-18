//! The health sweep (worker job): hourly checks on live domains.
//!
//! DNS still points at us (connected domains only — purchased zones we
//! control), the certificate is valid and matching, the storefront
//! answers with HTTP 200 and a sane redirect chain. Drift → `degraded`
//! with the exact failure code; recovery → back to `live`. A mass
//! degradation is logged loudly (it indicates our problem, not theirs).

use std::sync::Arc;

use sanvi_persistence::TxManager;

use crate::domain::{
    CertificateRepository, Clock, CustomDomain, CustomDomainRepository, DnsVerifierPort,
    DomainErrorCode, DomainKind, DomainsError, DomainsEvent, EventPublisher, StorefrontProbePort,
    storage,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealthReport {
    pub checked: u32,
    pub degraded: u32,
    pub recovered: u32,
}

pub struct RunHealthSweep {
    domains: Arc<dyn CustomDomainRepository>,
    certs: Arc<dyn CertificateRepository>,
    verifier: Arc<dyn DnsVerifierPort>,
    prober: Arc<dyn StorefrontProbePort>,
    events: Arc<dyn EventPublisher>,
    clock: Arc<dyn Clock>,
    tx: TxManager,
    edge_hostname: String,
    anycast_ips: Vec<String>,
}

impl RunHealthSweep {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        certs: Arc<dyn CertificateRepository>,
        verifier: Arc<dyn DnsVerifierPort>,
        prober: Arc<dyn StorefrontProbePort>,
        events: Arc<dyn EventPublisher>,
        clock: Arc<dyn Clock>,
        tx: TxManager,
        edge_hostname: String,
        anycast_ips: Vec<String>,
    ) -> Self {
        Self {
            domains,
            certs,
            verifier,
            prober,
            events,
            clock,
            tx,
            edge_hostname,
            anycast_ips,
        }
    }

    #[tracing::instrument(name = "domains.run_health_sweep", skip_all)]
    pub async fn execute(&self, limit: u32) -> Result<HealthReport, DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.health_sweep", "domains-worker")
            .await
            .map_err(storage)?;
        let live = self.domains.live_or_degraded(&mut scope, limit).await?;
        scope.commit().await.map_err(storage)?;

        let mut report = HealthReport {
            checked: live.len() as u32,
            ..HealthReport::default()
        };

        for mut domain in live {
            let mut scope = self
                .tx
                .begin_unscoped("domains.health_sweep.cert", "domains-worker")
                .await
                .map_err(storage)?;
            let cert = self
                .certs
                .find_for_domain_platform(&mut scope, domain.id)
                .await?;
            scope.commit().await.map_err(storage)?;

            let now = self.clock.now();
            let mut problem: Option<(&'static str, String)> = None;

            // Certificate valid and unexpired?
            match cert {
                Some(cert) if cert.days_to_expiry(now) > 0 => {}
                Some(_) => {
                    problem = Some((
                        DomainErrorCode::CERT_INVALID,
                        "the certificate is expired or missing validity".to_string(),
                    ));
                }
                None => {
                    problem = Some((
                        DomainErrorCode::CERT_INVALID,
                        "no certificate is recorded for this domain".to_string(),
                    ));
                }
            }

            // DNS still points at us? (connected domains only)
            if problem.is_none() && domain.kind == DomainKind::Connected {
                match self
                    .verifier
                    .verify_routing(&domain.hostname, &self.edge_hostname, &self.anycast_ips)
                    .await
                {
                    Ok(result) if result.confirmed => {}
                    Ok(result) => {
                        problem = Some((DomainErrorCode::DNS_DRIFT, result.detail));
                    }
                    Err(err) => {
                        problem = Some((
                            DomainErrorCode::DNS_DRIFT,
                            format!("routing check failed: {err}"),
                        ));
                    }
                }
            }

            // Storefront answers?
            if problem.is_none() {
                match self.prober.probe(&domain.hostname).await {
                    Ok(result) if result.ok => {}
                    Ok(result) => {
                        problem = Some((DomainErrorCode::PROBE_FAILED, result.detail));
                    }
                    Err(err) => {
                        problem = Some((DomainErrorCode::PROBE_FAILED, err.to_string()));
                    }
                }
            }

            match problem {
                Some((code, detail)) => {
                    self.degrade(&mut domain, code, detail, &mut report).await?
                }
                None => self.recover(&mut domain, &mut report).await?,
            }
        }

        if report.degraded >= 10 {
            tracing::error!(
                degraded = report.degraded,
                "mass domain degradation detected — likely a platform-side problem"
            );
        }
        Ok(report)
    }

    async fn degrade(
        &self,
        domain: &mut CustomDomain,
        code: &str,
        detail: String,
        report: &mut HealthReport,
    ) -> Result<(), DomainsError> {
        let was_healthy = domain.status == crate::domain::DomainStatus::Live;
        domain.mark_degraded(code, detail.clone(), self.clock.now())?;
        let mut scope = self
            .tx
            .begin_unscoped("domains.health_sweep.degrade", "domains-worker")
            .await
            .map_err(storage)?;
        self.domains.update_platform(&mut scope, domain).await?;
        self.domains
            .upsert_live_host(
                &mut scope,
                domain.tenant_id,
                domain.hostname.as_str(),
                domain.status.as_str(),
            )
            .await?;
        if was_healthy {
            self.events
                .publish(
                    &mut scope,
                    domain.tenant_id,
                    &[DomainsEvent::DomainDegraded {
                        domain_id: domain.id,
                        hostname: domain.hostname.as_str().to_string(),
                        code: code.to_string(),
                        detail: detail.clone(),
                    }],
                )
                .await?;
        }
        scope.commit().await.map_err(storage)?;
        if was_healthy {
            report.degraded += 1;
        }
        Ok(())
    }

    async fn recover(
        &self,
        domain: &mut CustomDomain,
        report: &mut HealthReport,
    ) -> Result<(), DomainsError> {
        let was_degraded = domain.status == crate::domain::DomainStatus::Degraded;
        if !was_degraded {
            // Nothing changed: avoid a pointless write (and the illegal
            // live → live transition).
            return Ok(());
        }
        domain.mark_live(self.clock.now())?;
        let mut scope = self
            .tx
            .begin_unscoped("domains.health_sweep.recover", "domains-worker")
            .await
            .map_err(storage)?;
        self.domains.update_platform(&mut scope, domain).await?;
        self.domains
            .upsert_live_host(
                &mut scope,
                domain.tenant_id,
                domain.hostname.as_str(),
                domain.status.as_str(),
            )
            .await?;
        scope.commit().await.map_err(storage)?;
        report.recovered += 1;
        Ok(())
    }
}
