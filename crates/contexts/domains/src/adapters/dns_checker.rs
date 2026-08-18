//! The DoH DNS verifier: multi-resolver agreement checks.
//!
//! Verification never trusts a single resolver — a resolver's cache can
//! lie, a resolver can be wrong, and during propagation different
//! resolvers genuinely disagree. `DnsCheckResult.confirmed` requires
//! agreement from at least `min_agreement` resolvers.
//!
//! The wire format is the Cloudflare/Google DoH JSON API (`dns-query`).

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::domain::{DnsCheckResult, DnsVerifierPort, DomainsError, Hostname};

#[derive(Debug, Deserialize)]
struct DohResponse {
    #[serde(rename = "Answer", default)]
    answer: Vec<DohAnswer>,
}

#[derive(Debug, Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    kind: i32,
    #[serde(rename = "data")]
    data: String,
}

const TYPE_TXT: i32 = 16;
const TYPE_CNAME: i32 = 5;
const TYPE_A: i32 = 1;
const TYPE_NS: i32 = 2;

pub struct DohDnsVerifier {
    client: reqwest::Client,
    resolvers: Vec<String>,
    min_agreement: usize,
}

impl DohDnsVerifier {
    pub fn new(resolvers: Vec<String>, min_agreement: usize, timeout_secs: u64) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("sanvi-domains-verifier/1.0")
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .expect("static reqwest client construction is infallible");
        Self {
            client,
            resolvers,
            min_agreement,
        }
    }

    async fn query(&self, name: &str, kind: i32) -> Result<Vec<Vec<String>>, DomainsError> {
        let mut results = Vec::new();
        let mut failures = 0;
        for resolver in &self.resolvers {
            match self.query_one(resolver, name, kind).await {
                Ok(answers) => results.push(answers),
                Err(_) => failures += 1,
            }
        }
        if results.is_empty() {
            return Err(DomainsError::VerificationUnavailable(format!(
                "all {failures} DoH resolvers failed for {name}"
            )));
        }
        Ok(results)
    }

    async fn query_one(
        &self,
        resolver: &str,
        name: &str,
        kind: i32,
    ) -> Result<Vec<String>, DomainsError> {
        let response = self
            .client
            .get(resolver)
            .query(&[("name", name), ("type", &kind.to_string())])
            .send()
            .await
            .map_err(|err| DomainsError::VerificationUnavailable(err.to_string()))?
            .error_for_status()
            .map_err(|err| DomainsError::VerificationUnavailable(err.to_string()))?;
        let payload: DohResponse = response
            .json()
            .await
            .map_err(|err| DomainsError::VerificationUnavailable(err.to_string()))?;
        // NXDOMAIN is an answer too: no records.
        Ok(payload
            .answer
            .into_iter()
            .filter(|answer| answer.kind == kind)
            .map(|answer| answer.data)
            .collect())
    }

    fn agreed(
        &self,
        per_resolver: &[Vec<String>],
        accept: impl Fn(&str) -> bool,
    ) -> DnsCheckResult {
        let checked = per_resolver.len();
        let agreed = per_resolver
            .iter()
            .filter(|answers| answers.iter().any(|data| accept(data)))
            .count();
        DnsCheckResult {
            confirmed: agreed >= self.min_agreement,
            resolvers_checked: checked,
            resolvers_agreed: agreed,
            detail: format!("{agreed}/{checked} resolvers agreed"),
        }
    }
}

#[async_trait]
impl DnsVerifierPort for DohDnsVerifier {
    async fn verify_txt(
        &self,
        hostname: &Hostname,
        expected_value: &str,
    ) -> Result<DnsCheckResult, DomainsError> {
        let name = format!("_sanvi-challenge.{hostname}");
        let per_resolver = self.query(&name, TYPE_TXT).await?;
        Ok(self.agreed(&per_resolver, |data| data == expected_value))
    }

    async fn verify_routing(
        &self,
        hostname: &Hostname,
        cname_target: &str,
        anycast_ips: &[String],
    ) -> Result<DnsCheckResult, DomainsError> {
        if hostname.is_apex() {
            let per_resolver = self.query(hostname.as_str(), TYPE_A).await?;
            Ok(self.agreed(&per_resolver, |data| {
                anycast_ips.iter().any(|ip| ip == data)
            }))
        } else {
            let per_resolver = self.query(hostname.as_str(), TYPE_CNAME).await?;
            Ok(self.agreed(&per_resolver, |data| {
                data.trim_end_matches('.') == cname_target
            }))
        }
    }

    async fn nameservers(&self, hostname: &Hostname) -> Result<Option<Vec<String>>, DomainsError> {
        // NS records live on the zone, not the hostname: ask the
        // registrable domain.
        let per_resolver = self.query(hostname.registrable(), TYPE_NS).await?;
        for answers in &per_resolver {
            if !answers.is_empty() {
                return Ok(Some(answers.clone()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agreement_requires_a_quorum() {
        let checker = DohDnsVerifier::new(Vec::new(), 2, 5);
        let result = checker.agreed(&[], |_| true);
        assert!(!result.confirmed);
    }

    #[test]
    fn conflicting_resolvers_do_not_confirm() {
        let checker = DohDnsVerifier::new(Vec::new(), 2, 5);
        let result = checker.agreed(
            &[vec!["sanvi-verify=abc".to_string()], Vec::new(), Vec::new()],
            |_| true,
        );
        assert!(!result.confirmed);
        assert_eq!(result.resolvers_agreed, 1);
    }

    #[test]
    fn quorum_confirms() {
        let checker = DohDnsVerifier::new(Vec::new(), 2, 5);
        let result = checker.agreed(
            &[
                vec!["sanvi-verify=abc".to_string()],
                vec!["sanvi-verify=abc".to_string()],
                Vec::new(),
            ],
            |_| true,
        );
        assert!(result.confirmed);
    }

    #[test]
    fn one_resolver_cannot_form_a_two_resolver_quorum() {
        let checker = DohDnsVerifier::new(Vec::new(), 2, 5);
        let result = checker.agreed(&[vec!["sanvi-verify=abc".to_string()]], |_| true);
        assert!(!result.confirmed);
    }
}
