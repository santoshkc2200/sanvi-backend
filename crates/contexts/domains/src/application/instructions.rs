//! `GET /api/v1/tenant/domains/{id}/instructions` — the exact records the
//! tenant must create, plus a registrar-specific walkthrough.
//!
//! Records are data; the walkthrough is localized copy rendered through the
//! `GuideTranslator` port (phase 06). Registrar detection runs in the
//! verification worker from the zone's NS records; until the first check
//! the generic guide is shown.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_persistence::ScopedTx;

use crate::application::views::{GuideView, InstructionRecord};
use crate::domain::{
    CustomDomainRepository, DnsRecord, DnsRecordType, DomainKind, DomainsError, GuideTranslator,
    Hostname, RegistrarGuideId,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InstructionsView {
    pub domain_id: Uuid,
    pub hostname: String,
    pub kind: String,
    pub role: String,
    pub status: String,
    pub records: Vec<InstructionRecord>,
    pub guide: GuideView,
}

/// The routing records for a hostname: CNAME to the edge for subdomains,
/// A records to the anycast IPs for apex (CNAME at apex is illegal in DNS;
/// providers with ALIAS/ANAME support can use it instead — documented in
/// the localized guide).
pub fn routing_records(
    hostname: &Hostname,
    edge_hostname: &str,
    anycast_ips: &[String],
) -> Vec<DnsRecord> {
    if hostname.is_apex() {
        anycast_ips
            .iter()
            .map(|ip| DnsRecord::new("@", DnsRecordType::A, ip.clone(), 300))
            .collect()
    } else {
        vec![DnsRecord::new(
            hostname.first_label(),
            DnsRecordType::Cname,
            edge_hostname,
            300,
        )]
    }
}

/// The verification TXT record.
pub fn challenge_record(token: &str) -> DnsRecord {
    DnsRecord::new(
        "_sanvi-challenge".to_string(),
        DnsRecordType::Txt,
        format!(
            "{}{}",
            crate::domain::records::VERIFICATION_TXT_PREFIX,
            token
        ),
        300,
    )
}

pub struct GetDomainInstructions {
    domains: Arc<dyn CustomDomainRepository>,
    translator: Arc<dyn GuideTranslator>,
    edge_hostname: String,
    anycast_ips: Vec<String>,
}

impl GetDomainInstructions {
    pub fn new(
        domains: Arc<dyn CustomDomainRepository>,
        translator: Arc<dyn GuideTranslator>,
        edge_hostname: String,
        anycast_ips: Vec<String>,
    ) -> Self {
        Self {
            domains,
            translator,
            edge_hostname,
            anycast_ips,
        }
    }

    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: Uuid,
        domain_id: Uuid,
        locale: &str,
    ) -> Result<InstructionsView, DomainsError> {
        let domain = self
            .domains
            .find(scope, tenant_id, domain_id)
            .await?
            .ok_or(DomainsError::UnknownDomain(domain_id))?;

        let guide_id = domain
            .detected_registrar
            .as_deref()
            .and_then(RegistrarGuideId::parse)
            .unwrap_or(RegistrarGuideId::Generic);

        let mut records = Vec::new();
        match domain.kind {
            DomainKind::Connected => {
                if let Some(challenge) = &domain.challenge {
                    records.push(challenge_record(&challenge.token));
                }
                records.extend(routing_records(
                    &domain.hostname,
                    &self.edge_hostname,
                    &self.anycast_ips,
                ));
            }
            DomainKind::Purchased => {
                // We manage purchased domains' DNS; instructions explain
                // what was configured, with no action required.
                records.extend(routing_records(
                    &domain.hostname,
                    &self.edge_hostname,
                    &self.anycast_ips,
                ));
            }
        }

        let explanations: Vec<String> = records
            .iter()
            .map(|record| match record.record_type {
                DnsRecordType::Txt => self.translator.translate(
                    locale,
                    "domains.records.txt.explanation",
                    &[("hostname", domain.hostname.as_str().to_string())],
                ),
                DnsRecordType::Cname => self.translator.translate(
                    locale,
                    "domains.records.cname.explanation",
                    &[("hostname", domain.hostname.as_str().to_string())],
                ),
                DnsRecordType::A => self.translator.translate(
                    locale,
                    "domains.records.apex.explanation",
                    &[("hostname", domain.hostname.as_str().to_string())],
                ),
                _ => self
                    .translator
                    .translate(locale, "domains.records.generic.explanation", &[]),
            })
            .collect();

        let guide_key = |suffix: &str| format!("domains.guides.{}.{suffix}", guide_id.as_str());
        let steps: Vec<String> = (1..=4)
            .map(|step| {
                self.translator.translate(
                    locale,
                    &guide_key(&format!("step_{step}")),
                    &[("hostname", domain.hostname.as_str().to_string())],
                )
            })
            .collect();
        let guide = GuideView {
            id: guide_id.as_str().to_string(),
            title: self.translator.translate(locale, &guide_key("title"), &[]),
            steps,
        };

        Ok(InstructionsView {
            domain_id: domain.id,
            hostname: domain.hostname.as_str().to_string(),
            kind: domain.kind.as_str().to_string(),
            role: domain.role.as_str().to_string(),
            status: domain.status.as_str().to_string(),
            records: records
                .into_iter()
                .zip(explanations)
                .map(|(record, explanation)| InstructionRecord::of(&record, explanation))
                .collect(),
            guide,
        })
    }
}
