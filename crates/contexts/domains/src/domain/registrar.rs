//! The registrar port and its normalized result types.
//!
//! Pricing, TLD availability and renewal semantics are normalized across
//! providers; the registrar choice is commercial and will change — adapters
//! implement this port. The reference deployment ships the sandbox adapter
//! (deterministic, no external accounts); a production adapter is a port
//! swap.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::domain::{DomainsError, Hostname, Money, TldPrice};

/// One search hit: a hostname candidate with normalized pricing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DomainQuote {
    pub hostname: String,
    pub tld: String,
    pub available: bool,
    pub premium: bool,
    pub register_price: Money,
    pub renew_price: Money,
    pub registrar_id: String,
}

/// The registrar's confirmation of a registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RegistrationResult {
    pub external_ref: String,
    pub registered_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The registrar's confirmation of a renewal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RenewalResult {
    pub expires_at: DateTime<Utc>,
}

/// Registrar-guide ids the instruction endpoint can localize. The
/// verification worker derives one from the zone's NS records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistrarGuideId {
    Generic,
    Cloudflare,
    Route53,
    GoDaddy,
    Namecheap,
    GoogleDomains,
    OnamaeJp,
    ValueDomain,
}

impl RegistrarGuideId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Cloudflare => "cloudflare",
            Self::Route53 => "route53",
            Self::GoDaddy => "godaddy",
            Self::Namecheap => "namecheap",
            Self::GoogleDomains => "google_domains",
            Self::OnamaeJp => "onamae_jp",
            Self::ValueDomain => "value_domain",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "generic" => Self::Generic,
            "cloudflare" => Self::Cloudflare,
            "route53" => Self::Route53,
            "godaddy" => Self::GoDaddy,
            "namecheap" => Self::Namecheap,
            "google_domains" => Self::GoogleDomains,
            "onamae_jp" => Self::OnamaeJp,
            "value_domain" => Self::ValueDomain,
            _ => return None,
        })
    }

    /// Heuristic NS-suffix → guide mapping. Pure so it is exhaustively
    /// unit-testable.
    pub fn from_nameservers(nameservers: &[String]) -> Self {
        let lower: Vec<String> = nameservers
            .iter()
            .map(|ns| ns.to_ascii_lowercase())
            .collect();
        let any = |suffix: &str| lower.iter().any(|ns| ns.ends_with(suffix));
        if any("ns.cloudflare.com") || any("cloudflare.com") {
            Self::Cloudflare
        } else if lower.iter().any(|ns| ns.contains("awsdns")) || any("amazonaws.com") {
            Self::Route53
        } else if any("domaincontrol.com") {
            Self::GoDaddy
        } else if any("namecheaphosting.com") || any("registrar-servers.com") {
            Self::Namecheap
        } else if any("googledomains.com") {
            Self::GoogleDomains
        } else if any("gmoserver.jp") || any("dns.ne.jp") {
            Self::OnamaeJp
        } else if any("value-domain.com") || any("dnsv.jp") {
            Self::ValueDomain
        } else {
            Self::Generic
        }
    }
}

/// The registrar gateway: search, quote, register, renew.
///
/// Contract: registration is idempotent on the order id — a retried call
/// for the same order must return the same external reference, never a
/// second registration.
#[async_trait]
pub trait DomainRegistrarPort: Send + Sync {
    /// The registrar's stable id (recorded on orders and quotes).
    fn registrar_id(&self) -> String;

    /// The normalized TLD catalog this registrar serves.
    fn catalog(&self) -> Vec<TldPrice>;

    /// Search: availability + pricing for candidate hostnames under the
    /// served TLDs. `query` is a bare term (`acme`) or a hostname
    /// (`acme.com`); results are filtered to served TLDs.
    async fn search(&self, query: &str) -> Result<Vec<DomainQuote>, DomainsError>;

    /// Re-quote one hostname immediately before purchase (the price shown
    /// at checkout is a promise, not a snapshot).
    async fn quote(&self, hostname: &Hostname, term_years: u8)
    -> Result<DomainQuote, DomainsError>;

    /// Register. Idempotent on `order_id`; fails if the domain is taken.
    async fn register(
        &self,
        order_id: Uuid,
        hostname: &Hostname,
        term_years: u8,
        whois_privacy: bool,
    ) -> Result<RegistrationResult, DomainsError>;

    /// Renew an existing registration for `term_years` more.
    async fn renew(
        &self,
        external_ref: &str,
        hostname: &Hostname,
        term_years: u8,
    ) -> Result<RenewalResult, DomainsError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nameserver_heuristics_detect_registrars() {
        assert_eq!(
            RegistrarGuideId::from_nameservers(&["ada.ns.cloudflare.com".to_string()]),
            RegistrarGuideId::Cloudflare
        );
        assert_eq!(
            RegistrarGuideId::from_nameservers(&["ns-42.awsdns-05.com".to_string()]),
            RegistrarGuideId::Route53
        );
        assert_eq!(
            RegistrarGuideId::from_nameservers(&["pdns05.domaincontrol.com".to_string()]),
            RegistrarGuideId::GoDaddy
        );
        assert_eq!(
            RegistrarGuideId::from_nameservers(&["dns1.namecheaphosting.com".to_string()]),
            RegistrarGuideId::Namecheap
        );
        assert_eq!(
            RegistrarGuideId::from_nameservers(&[
                "01.dnsv.jp".to_string(),
                "02.dnsv.jp".to_string()
            ]),
            RegistrarGuideId::ValueDomain
        );
        assert_eq!(
            RegistrarGuideId::from_nameservers(&["ns1.random-host.net".to_string()]),
            RegistrarGuideId::Generic
        );
    }
}
