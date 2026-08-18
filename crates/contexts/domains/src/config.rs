//! The domains context's configuration.
//!
//! Reference deployment: the registrar, DNS provider and certificate
//! authority run in sandbox mode (deterministic, no external accounts);
//! production adapters implement the same ports. Everything that could
//! fail closed does: an empty `internal_secret` denies every TLS
//! authorization request.

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Pricing for one TLD in the sandbox registrar's catalog.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TldConfig {
    /// The TLD, without a dot: `com`, `dev`, `jp`.
    pub tld: String,
    /// First-year registration price in minor units of `currency`.
    pub register_price_minor: i64,
    /// Yearly renewal price in minor units of `currency`.
    pub renew_price_minor: i64,
    /// ISO 4217 currency code.
    pub currency: String,
}

/// The registrar port's reference (sandbox) adapter configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RegistrarConfig {
    /// `sandbox` today; a production adapter is selected by changing this.
    pub mode: String,
    /// The searchable/registrable TLD catalog with normalized pricing.
    #[serde(default)]
    pub tlds: Vec<TldConfig>,
    /// How long search results are cached in the API process (seconds).
    #[serde(default = "default_search_cache_ttl")]
    pub search_cache_ttl_secs: u64,
    /// The registrar id reported on orders (`sandbox`).
    #[serde(default = "default_registrar_id")]
    pub registrar_id: String,
}

fn default_search_cache_ttl() -> u64 {
    300
}

fn default_registrar_id() -> String {
    "sandbox".to_string()
}

impl Default for RegistrarConfig {
    fn default() -> Self {
        Self {
            mode: "sandbox".to_string(),
            tlds: vec![
                TldConfig {
                    tld: "com".to_string(),
                    register_price_minor: 1299,
                    renew_price_minor: 1599,
                    currency: "usd".to_string(),
                },
                TldConfig {
                    tld: "net".to_string(),
                    register_price_minor: 1399,
                    renew_price_minor: 1699,
                    currency: "usd".to_string(),
                },
                TldConfig {
                    tld: "dev".to_string(),
                    register_price_minor: 1499,
                    renew_price_minor: 1499,
                    currency: "usd".to_string(),
                },
                TldConfig {
                    tld: "jp".to_string(),
                    register_price_minor: 2400,
                    renew_price_minor: 2400,
                    currency: "usd".to_string(),
                },
            ],
            search_cache_ttl_secs: default_search_cache_ttl(),
            registrar_id: default_registrar_id(),
        }
    }
}

/// The domains context's configuration.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DomainsConfig {
    /// The platform's own domain; tenant hostnames must not be this domain
    /// or any subdomain of it.
    pub platform_domain: String,
    /// The edge hostname CNAME records point at (`edge.sanvi.app`).
    pub edge_hostname: String,
    /// The anycast IPs A records for apex domains fall back to.
    pub anycast_ips: Vec<String>,
    /// DoH JSON endpoints (Cloudflare/Google style) for verification.
    pub doh_resolvers: Vec<String>,
    /// Verification requires agreement from at least this many resolvers.
    #[serde(default = "default_doh_min_agreement")]
    pub doh_min_agreement: usize,
    /// Per-resolver DoH timeout.
    #[serde(default = "default_doh_timeout")]
    pub doh_timeout_secs: u64,
    /// A claim expires (is removed) after this long unverified, so a
    /// hostname is never permanently squatted.
    #[serde(default = "default_verification_timeout")]
    pub verification_timeout_hours: u64,
    /// Verification polling backoff: start and cap.
    #[serde(default = "default_backoff_base")]
    pub verification_backoff_base_secs: u64,
    #[serde(default = "default_backoff_max")]
    pub verification_backoff_max_secs: u64,
    /// Sandbox CA certificate validity.
    #[serde(default = "default_cert_lifetime")]
    pub cert_lifetime_days: u32,
    /// Renewal starts this many days before `not_after`.
    #[serde(default = "default_renewal_before")]
    pub cert_renewal_before_days: u32,
    /// Expiry warnings are emitted at these days-left thresholds.
    #[serde(default = "default_expiry_warnings")]
    pub cert_expiry_warning_days: Vec<u32>,
    /// Host→tenant cache TTL used by the tenancy resolution hook.
    #[serde(default = "default_host_cache_ttl")]
    pub host_cache_ttl_secs: u64,
    /// Shared secret the edge presents on `/internal/tls/authorize`.
    /// Empty means the gate fails closed.
    #[serde(default)]
    pub internal_secret: String,
    /// Optional CIDR allow-list for `/internal` domains routes.
    #[serde(default)]
    pub internal_allowed_ips: Vec<String>,
    /// Storefront probe timeout.
    #[serde(default = "default_probe_timeout")]
    pub prober_timeout_secs: u64,
    #[serde(default = "default_probe_ua")]
    pub prober_user_agent: String,
    /// Auto-renewal: orders renew this many days before registry expiry.
    #[serde(default = "default_renewal_lead")]
    pub order_renewal_lead_days: u32,
    /// Auto-renewal retry window between attempts (seconds).
    #[serde(default = "default_renewal_retry")]
    pub order_renewal_retry_secs: u64,
    /// Maximum auto-renewal charge/registration attempts before escalation.
    #[serde(default = "default_renewal_attempts")]
    pub order_renewal_max_attempts: u32,
    /// Entitlement keys gating the two journeys.
    #[serde(default = "default_connect_entitlement")]
    pub connect_entitlement_key: String,
    #[serde(default = "default_purchase_entitlement")]
    pub purchase_entitlement_key: String,
    #[serde(default)]
    pub registrar: RegistrarConfig,
}

fn default_doh_min_agreement() -> usize {
    2
}

fn default_doh_timeout() -> u64 {
    5
}

fn default_verification_timeout() -> u64 {
    72
}

fn default_backoff_base() -> u64 {
    10
}

fn default_backoff_max() -> u64 {
    300
}

fn default_cert_lifetime() -> u32 {
    90
}

fn default_renewal_before() -> u32 {
    30
}

fn default_expiry_warnings() -> Vec<u32> {
    vec![30, 14, 7, 1]
}

fn default_host_cache_ttl() -> u64 {
    300
}

fn default_probe_timeout() -> u64 {
    10
}

fn default_probe_ua() -> String {
    "sanvi-domain-health/1.0".to_string()
}

fn default_renewal_lead() -> u32 {
    30
}

fn default_renewal_retry() -> u64 {
    86400
}

fn default_renewal_attempts() -> u32 {
    3
}

fn default_connect_entitlement() -> String {
    "domains.custom".to_string()
}

fn default_purchase_entitlement() -> String {
    "domains.purchase".to_string()
}

impl Default for DomainsConfig {
    fn default() -> Self {
        Self {
            platform_domain: "platform.sanvi.app".to_string(),
            edge_hostname: "edge.sanvi.app".to_string(),
            anycast_ips: vec!["203.0.113.10".to_string(), "203.0.113.11".to_string()],
            doh_resolvers: vec![
                "https://cloudflare-dns.com/dns-query".to_string(),
                "https://dns.google/resolve".to_string(),
            ],
            doh_min_agreement: default_doh_min_agreement(),
            doh_timeout_secs: default_doh_timeout(),
            verification_timeout_hours: default_verification_timeout(),
            verification_backoff_base_secs: default_backoff_base(),
            verification_backoff_max_secs: default_backoff_max(),
            cert_lifetime_days: default_cert_lifetime(),
            cert_renewal_before_days: default_renewal_before(),
            cert_expiry_warning_days: default_expiry_warnings(),
            host_cache_ttl_secs: default_host_cache_ttl(),
            internal_secret: String::new(),
            internal_allowed_ips: Vec::new(),
            prober_timeout_secs: default_probe_timeout(),
            prober_user_agent: default_probe_ua(),
            order_renewal_lead_days: default_renewal_lead(),
            order_renewal_retry_secs: default_renewal_retry(),
            order_renewal_max_attempts: default_renewal_attempts(),
            connect_entitlement_key: default_connect_entitlement(),
            purchase_entitlement_key: default_purchase_entitlement(),
            registrar: RegistrarConfig::default(),
        }
    }
}

impl DomainsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.platform_domain.trim().is_empty() {
            anyhow::bail!("domains.platform_domain must not be empty");
        }
        if self.edge_hostname.trim().is_empty() {
            anyhow::bail!("domains.edge_hostname must not be empty");
        }
        if self.anycast_ips.is_empty() {
            anyhow::bail!("domains.anycast_ips must have at least one entry");
        }
        for ip in &self.anycast_ips {
            ip.parse::<std::net::IpAddr>()
                .context(format!("domains.anycast_ips contains a non-IP value: {ip}"))?;
        }
        if self.doh_resolvers.is_empty() && self.doh_min_agreement > 0 {
            // Verification is impossible; fail loudly rather than silently
            // never verifying anything.
            anyhow::bail!("domains.doh_resolvers must not be empty");
        }
        if self.doh_min_agreement == 0 {
            anyhow::bail!("domains.doh_min_agreement must be >= 1");
        }
        if self.doh_min_agreement > self.doh_resolvers.len() {
            anyhow::bail!("domains.doh_min_agreement exceeds the number of resolvers");
        }
        if self.verification_timeout_hours == 0 {
            anyhow::bail!("domains.verification_timeout_hours must be >= 1");
        }
        if self.verification_backoff_base_secs == 0 {
            anyhow::bail!("domains.verification_backoff_base_secs must be >= 1");
        }
        if self.verification_backoff_max_secs < self.verification_backoff_base_secs {
            anyhow::bail!("domains.verification_backoff_max_secs must be >= the base");
        }
        if self.cert_lifetime_days == 0 || self.cert_renewal_before_days == 0 {
            anyhow::bail!("domains.cert_lifetime_days and cert_renewal_before_days must be >= 1");
        }
        if self.cert_renewal_before_days >= self.cert_lifetime_days {
            anyhow::bail!("domains.cert_renewal_before_days must be < cert_lifetime_days");
        }
        if self.cert_expiry_warning_days.is_empty() {
            anyhow::bail!("domains.cert_expiry_warning_days must not be empty");
        }
        if self.order_renewal_max_attempts == 0 {
            anyhow::bail!("domains.order_renewal_max_attempts must be >= 1");
        }
        if self.registrar.mode != "sandbox" {
            anyhow::bail!(
                "domains.registrar.mode must be \"sandbox\" (production adapters are not built yet)"
            );
        }
        if self.registrar.tlds.is_empty() {
            anyhow::bail!("domains.registrar.tlds must not be empty");
        }
        for tld in &self.registrar.tlds {
            if tld.tld.is_empty() || tld.tld.starts_with('.') || tld.tld.contains('.') {
                anyhow::bail!(
                    "domains.registrar.tlds contains an invalid TLD: {}",
                    tld.tld
                );
            }
            if tld.register_price_minor <= 0 || tld.renew_price_minor <= 0 {
                anyhow::bail!(
                    "domains.registrar.tlds prices must be positive: {}",
                    tld.tld
                );
            }
            if tld.currency.is_empty() {
                anyhow::bail!(
                    "domains.registrar.tlds currency must not be empty: {}",
                    tld.tld
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        DomainsConfig::default()
            .validate()
            .expect("defaults validate");
    }

    #[test]
    fn rejects_empty_resolver_list() {
        let config = DomainsConfig {
            doh_resolvers: Vec::new(),
            ..DomainsConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_bad_anycast_ip() {
        let config = DomainsConfig {
            anycast_ips: vec!["not-an-ip".to_string()],
            ..DomainsConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
