use std::sync::Arc;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use sanvi_kernel::TenantId;
use sanvi_persistence::ScopedTx;

use crate::application::views::{GuideView, InstructionRecord};
use crate::domain::{
    CustomDomainId, CustomDomainRepository, DnsRecord, DnsRecordType, DomainKind, DomainsError,
    GuideTranslator, Hostname, RegistrarGuideId,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InstructionsView {
    #[schema(value_type = String)]
    pub domain_id: CustomDomainId,
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
            zone_relative_name(hostname),
            DnsRecordType::Cname,
            edge_hostname,
            300,
        )]
    }
}

/// The verification TXT record.
pub fn challenge_record(hostname: &Hostname, token: &str) -> DnsRecord {
    let name = if hostname.is_apex() {
        "_sanvi-challenge".to_string()
    } else {
        format!("_sanvi-challenge.{}", zone_relative_name(hostname))
    };
    DnsRecord::new(
        name,
        DnsRecordType::Txt,
        format!(
            "{}{}",
            crate::domain::records::VERIFICATION_TXT_PREFIX,
            token
        ),
        300,
    )
}

fn zone_relative_name(hostname: &Hostname) -> &str {
    hostname
        .as_str()
        .strip_suffix(hostname.registrable())
        .unwrap_or_default()
        .trim_end_matches('.')
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

    #[tracing::instrument(name = "domains.get_domain_instructions", skip_all, fields(tenant_id = %tenant_id))]
    pub async fn execute(
        &self,
        scope: &mut ScopedTx<'_>,
        tenant_id: TenantId,
        domain_id: CustomDomainId,
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
                    records.push(challenge_record(&domain.hostname, &challenge.token));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdomain_challenge_is_relative_to_registrable_zone() {
        let host = Hostname::parse("shop.eu.example.co.uk").unwrap();
        assert_eq!(
            challenge_record(&host, "token").name,
            "_sanvi-challenge.shop.eu"
        );
        assert_eq!(
            routing_records(&host, "edge.sanvi.app", &[])[0].name,
            "shop.eu"
        );
    }

    #[test]
    fn apex_challenge_and_routing_keep_zone_names() {
        let host = Hostname::parse("example.com").unwrap();
        assert_eq!(challenge_record(&host, "token").name, "_sanvi-challenge");
        assert_eq!(
            routing_records(&host, "edge.sanvi.app", &["8.8.8.8".to_string()])[0].name,
            "@"
        );
    }
}
