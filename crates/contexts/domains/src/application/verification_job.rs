//! The verification sweep (worker job): polls due domains with backoff,
//! checks ownership (TXT) and routing (CNAME/A) independently, and
//! promotes verified claims — or closes expired/duplicate ones.
//!
//! Checker selection: connected domains are checked against multiple
//! DoH resolvers (agreement ≥ `doh_min_agreement`); purchased domains are
//! checked against the records the DNS provider applied (we control the
//! zone — asking the internet would only add latency).

use std::sync::Arc;

use chrono::{DateTime, Utc};

use sanvi_persistence::TxManager;

use crate::domain::{
    CustomDomain, CustomDomainRepository, DnsProviderPort, DnsVerifierPort, DomainErrorCode,
    DomainKind, DomainsError, DomainsEvent, EventPublisher, RegistrarGuideId, storage,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VerificationReport {
    pub checked: u32,
    pub verified: u32,
    pub failed: u32,
    pub expired: u32,
    pub superseded: u32,
}

impl VerificationReport {
    fn merge(&mut self, other: &Self) {
        self.checked += other.checked;
        self.verified += other.verified;
        self.failed += other.failed;
        self.expired += other.expired;
        self.superseded += other.superseded;
    }
}

pub struct RunVerificationSweep {
    domains: Arc<dyn CustomDomainRepository>,
    verifier: Arc<dyn DnsVerifierPort>,
    provider: Arc<dyn DnsProviderPort>,
    events: Arc<dyn EventPublisher>,
    tx: TxManager,
    edge_hostname: String,
    anycast_ips: Vec<String>,
    backoff_base_secs: u64,
    backoff_max_secs: u64,
}

impl RunVerificationSweep {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        verifier: Arc<dyn DnsVerifierPort>,
        provider: Arc<dyn DnsProviderPort>,
        events: Arc<dyn EventPublisher>,
        tx: TxManager,
        edge_hostname: String,
        anycast_ips: Vec<String>,
        backoff_base_secs: u64,
        backoff_max_secs: u64,
    ) -> Self {
        Self {
            domains,
            verifier,
            provider,
            events,
            tx,
            edge_hostname,
            anycast_ips,
            backoff_base_secs,
            backoff_max_secs,
        }
    }

    pub async fn execute(
        &self,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<VerificationReport, DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.verification_sweep", "domains-worker")
            .await
            .map_err(storage)?;
        let due = self
            .domains
            .due_for_verification(&mut scope, now, limit)
            .await?;
        scope.commit().await.map_err(storage)?;

        let mut report = VerificationReport::default();
        for mut domain in due {
            let one = self.check_one(now, &mut domain).await?;
            report.merge(&one);
        }
        Ok(report)
    }

    async fn check_one(
        &self,
        now: DateTime<Utc>,
        domain: &mut CustomDomain,
    ) -> Result<VerificationReport, DomainsError> {
        let mut report = VerificationReport {
            checked: 1,
            ..VerificationReport::default()
        };

        // Challenge expired: the claim closes; the hostname is free again
        // (a claim never permanently squats a hostname).
        let expired = domain
            .challenge
            .as_ref()
            .map(|challenge| challenge.expires_at <= now)
            .unwrap_or(true);
        if expired {
            domain.remove(
                DomainErrorCode::CHALLENGE_EXPIRED,
                "the verification challenge expired before DNS was confirmed".to_string(),
                now,
            )?;
            self.persist_domain(
                domain,
                &[DomainsEvent::DomainVerificationFailed {
                    domain_id: domain.id,
                    hostname: domain.hostname.as_str().to_string(),
                    code: DomainErrorCode::CHALLENGE_EXPIRED.to_string(),
                    detail: "challenge expired".to_string(),
                }],
            )
            .await?;
            report.expired = 1;
            return Ok(report);
        }

        let expected_txt = domain
            .challenge
            .as_ref()
            .map(|challenge| challenge.txt_value())
            .unwrap_or_default();

        // Registrar detection (connected domains): one NS lookup, cached on
        // the row, drives the localized guide.
        if domain.kind == DomainKind::Connected
            && domain.detected_registrar.is_none()
            && let Ok(Some(nameservers)) = self.verifier.nameservers(&domain.hostname).await
        {
            domain.detected_registrar = Some(
                RegistrarGuideId::from_nameservers(&nameservers)
                    .as_str()
                    .to_string(),
            );
        }

        // Ownership check.
        let txt_ok = match domain.kind {
            DomainKind::Connected => match self
                .verifier
                .verify_txt(&domain.hostname, &expected_txt)
                .await
            {
                Ok(result) => result.confirmed,
                Err(err) => {
                    tracing::warn!(
                        hostname = domain.hostname.as_str(),
                        error = %err,
                        "DNS verification failed at the resolver layer; will retry"
                    );
                    self.schedule_failure(domain, DomainErrorCode::TXT_MISSING, now)
                        .await?;
                    report.failed = 1;
                    return Ok(report);
                }
            },
            DomainKind::Purchased => {
                let records = self.provider_records(domain.id).await?;
                records.iter().any(|record| {
                    record.record_type == crate::domain::DnsRecordType::Txt
                        && record.value == expected_txt
                })
            }
        };

        // Routing check (independent: ownership and routing fail for
        // different reasons and the tenant must know which).
        let routing_ok = match domain.kind {
            DomainKind::Connected => match self
                .verifier
                .verify_routing(&domain.hostname, &self.edge_hostname, &self.anycast_ips)
                .await
            {
                Ok(result) => result.confirmed,
                Err(_) => false,
            },
            DomainKind::Purchased => {
                let records = self.provider_records(domain.id).await?;
                if domain.hostname.is_apex() {
                    records.iter().any(|record| {
                        record.record_type == crate::domain::DnsRecordType::A
                            && self.anycast_ips.contains(&record.value)
                    })
                } else {
                    records.iter().any(|record| {
                        record.record_type == crate::domain::DnsRecordType::Cname
                            && record.value == self.edge_hostname
                    })
                }
            }
        };

        match (txt_ok, routing_ok) {
            (true, true) => {
                // The DB partial unique index enforces "verified claim
                // wins": this update fails if another tenant verified the
                // hostname first.
                domain.mark_verified(now)?;
                if let Err(err) = self
                    .persist_domain(
                        domain,
                        &[DomainsEvent::DomainVerified {
                            domain_id: domain.id,
                            hostname: domain.hostname.as_str().to_string(),
                        }],
                    )
                    .await
                {
                    match err {
                        DomainsError::HostnameAlreadyClaimed(_) => {
                            domain.remove(
                                DomainErrorCode::VERIFIED_BY_OTHER_TENANT,
                                "another tenant verified this hostname first".to_string(),
                                now,
                            )?;
                            self.persist_domain_quiet(domain).await?;
                            report.superseded = 1;
                        }
                        other => return Err(other),
                    }
                } else {
                    report.verified = 1;
                }
            }
            (false, _) => {
                self.schedule_failure(domain, DomainErrorCode::TXT_MISSING, now)
                    .await?;
                report.failed = 1;
            }
            (true, false) => {
                self.schedule_failure(domain, DomainErrorCode::ROUTING_MISSING, now)
                    .await?;
                report.failed = 1;
            }
        }
        Ok(report)
    }

    async fn provider_records(
        &self,
        domain_id: uuid::Uuid,
    ) -> Result<Vec<crate::domain::DnsRecord>, DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.verification_sweep.records", "domains-worker")
            .await
            .map_err(storage)?;
        let records = self
            .provider
            .records_platform(&mut scope, domain_id)
            .await?;
        scope.commit().await.map_err(storage)?;
        Ok(records)
    }

    async fn schedule_failure(
        &self,
        domain: &mut CustomDomain,
        code: &str,
        now: DateTime<Utc>,
    ) -> Result<(), DomainsError> {
        domain.mark_verification_failed(
            code,
            "DNS does not yet reflect the required records".to_string(),
            now,
            self.backoff_base_secs,
            self.backoff_max_secs,
        )?;
        self.persist_domain_quiet(domain).await
    }

    async fn persist_domain(
        &self,
        domain: &CustomDomain,
        events: &[DomainsEvent],
    ) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.verification_sweep.persist", "domains-worker")
            .await
            .map_err(storage)?;
        self.domains.update_platform(&mut scope, domain).await?;
        // Entry into the live set maintains the resolution projection.
        if domain.status.is_live_claim() {
            self.domains
                .upsert_live_host(
                    &mut scope,
                    domain.tenant_id,
                    domain.hostname.as_str(),
                    domain.status.as_str(),
                )
                .await?;
        } else if domain.status == crate::domain::DomainStatus::Removed {
            self.domains
                .delete_live_host(&mut scope, domain.hostname.as_str())
                .await?;
        }
        self.events
            .publish(&mut scope, domain.tenant_id, events)
            .await?;
        scope.commit().await.map_err(storage)
    }

    async fn persist_domain_quiet(&self, domain: &CustomDomain) -> Result<(), DomainsError> {
        let mut scope = self
            .tx
            .begin_unscoped("domains.verification_sweep.persist", "domains-worker")
            .await
            .map_err(storage)?;
        self.domains.update_platform(&mut scope, domain).await?;
        if domain.status == crate::domain::DomainStatus::Removed {
            self.domains
                .delete_live_host(&mut scope, domain.hostname.as_str())
                .await?;
        }
        scope.commit().await.map_err(storage)
    }
}
