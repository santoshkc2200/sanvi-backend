//! The sandbox registrar: the reference `DomainRegistrarPort` adapter.
//!
//! Deterministic, config-driven, no external accounts: the TLD catalog and
//! prices come from `RegistrarConfig`; search filters the query against
//! the served TLDs; registration always succeeds (recording the order id
//! as the external reference — the idempotency contract) unless the
//! hostname is a reserved name. A production adapter implements the same
//! port.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{Datelike, Utc};
use uuid::Uuid;

use crate::config::TldConfig;
use crate::domain::{
    DomainQuote, DomainRegistrarPort, DomainsError, Hostname, Money, RegistrationResult,
    RenewalResult, TldPrice,
};

/// Names the sandbox never sells (mirrors the reserved hostname list).
const RESERVED_TERMS: &[&str] = &["localhost", "example", "invalid", "test", "paypal", "sanvi"];

pub struct SandboxRegistrar {
    registrar_id: String,
    tlds: Vec<TldPrice>,
    prices: HashMap<String, TldPrice>,
}

impl SandboxRegistrar {
    pub fn new(registrar_id: String, tld_configs: &[TldConfig]) -> Self {
        let tlds: Vec<TldPrice> = tld_configs
            .iter()
            .map(|config| TldPrice {
                tld: config.tld.clone(),
                register: Money::new(config.register_price_minor, config.currency.clone()),
                renew: Money::new(config.renew_price_minor, config.currency.clone()),
            })
            .collect();
        let prices = tlds
            .iter()
            .map(|price| (price.tld.clone(), price.clone()))
            .collect();
        Self {
            registrar_id,
            tlds,
            prices,
        }
    }

    fn available(&self, hostname: &Hostname) -> bool {
        let first = hostname.first_label();
        !RESERVED_TERMS
            .iter()
            .any(|reserved| first == *reserved || first.starts_with(reserved))
    }
}

#[async_trait]
impl DomainRegistrarPort for SandboxRegistrar {
    fn registrar_id(&self) -> String {
        self.registrar_id.clone()
    }

    fn catalog(&self) -> Vec<TldPrice> {
        self.tlds.clone()
    }

    async fn search(&self, query: &str) -> Result<Vec<DomainQuote>, DomainsError> {
        // A hostname-shaped query collapses to that one candidate; a bare
        // term fans out over the served TLDs.
        let candidates: Vec<Hostname> = if query.contains('.') {
            match Hostname::parse(query) {
                Ok(hostname) => vec![hostname],
                Err(_) => Vec::new(),
            }
        } else {
            let mut out = Vec::new();
            for price in &self.tlds {
                let name = format!("{query}.{}", price.tld);
                if let Ok(hostname) = Hostname::parse(&name) {
                    out.push(hostname);
                }
            }
            out
        };

        let mut results = Vec::new();
        for hostname in candidates {
            let tld = hostname
                .as_str()
                .rsplit_once('.')
                .map(|(_, tld)| tld.to_string())
                .unwrap_or_default();
            let Some(price) = self.prices.get(&tld) else {
                continue;
            };
            let available = self.available(&hostname);
            results.push(DomainQuote {
                hostname: hostname.as_str().to_string(),
                tld,
                available,
                premium: false,
                register_price: price.register.clone(),
                renew_price: price.renew.clone(),
                registrar_id: self.registrar_id.clone(),
            });
        }
        Ok(results)
    }

    async fn quote(
        &self,
        hostname: &Hostname,
        term_years: u8,
    ) -> Result<DomainQuote, DomainsError> {
        let tld = hostname
            .as_str()
            .rsplit_once('.')
            .map(|(_, tld)| tld.to_string())
            .unwrap_or_default();
        let price = self
            .prices
            .get(&tld)
            .ok_or_else(|| DomainsError::UnsupportedTld(tld.clone()))?;
        let available = self.available(hostname);
        let _ = term_years;
        Ok(DomainQuote {
            hostname: hostname.as_str().to_string(),
            tld,
            available,
            premium: false,
            register_price: price.register.clone(),
            renew_price: price.renew.clone(),
            registrar_id: self.registrar_id.clone(),
        })
    }

    async fn register(
        &self,
        order_id: Uuid,
        hostname: &Hostname,
        term_years: u8,
        _whois_privacy: bool,
    ) -> Result<RegistrationResult, DomainsError> {
        if !self.available(hostname) {
            return Err(DomainsError::DomainUnavailable(
                hostname.as_str().to_string(),
            ));
        }
        let now = Utc::now();
        Ok(RegistrationResult {
            external_ref: format!("sandbox-{order_id}"),
            registered_at: now,
            expires_at: add_years(now, term_years),
        })
    }

    async fn renew(
        &self,
        _external_ref: &str,
        _hostname: &Hostname,
        term_years: u8,
    ) -> Result<RenewalResult, DomainsError> {
        let now = Utc::now();
        Ok(RenewalResult {
            expires_at: add_years(now, term_years),
        })
    }
}

/// Add `years` calendar years (Feb 29 safe-ish: clamps via `with_year`).
fn add_years(base: chrono::DateTime<Utc>, years: u8) -> chrono::DateTime<Utc> {
    let year = base.year() + years as i32;
    base.with_year(year).unwrap_or_else(|| {
        // Feb 29 → Feb 28.
        base.with_day(28)
            .and_then(|date| date.with_year(year))
            .unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TldConfig;

    fn registrar() -> SandboxRegistrar {
        SandboxRegistrar::new(
            "sandbox".to_string(),
            &[TldConfig {
                tld: "com".to_string(),
                register_price_minor: 1299,
                renew_price_minor: 1599,
                currency: "usd".to_string(),
            }],
        )
    }

    #[tokio::test]
    async fn search_fans_out_over_served_tlds() {
        let results = registrar().search("acme").await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hostname, "acme.com");
        assert!(results[0].available);
    }

    #[tokio::test]
    async fn reserved_names_are_unavailable() {
        let results = registrar().search("paypal").await.unwrap();
        assert!(!results[0].available);
    }

    #[tokio::test]
    async fn unsupported_tld_quote_errors() {
        let hostname = Hostname::parse("acme.io").unwrap();
        assert!(matches!(
            registrar().quote(&hostname, 1).await,
            Err(DomainsError::UnsupportedTld(_))
        ));
    }

    #[tokio::test]
    async fn registration_records_the_order_id() {
        let hostname = Hostname::parse("acme.com").unwrap();
        let order_id = Uuid::now_v7();
        let result = registrar()
            .register(order_id, &hostname, 1, true)
            .await
            .unwrap();
        assert_eq!(result.external_ref, format!("sandbox-{order_id}"));
    }
}
