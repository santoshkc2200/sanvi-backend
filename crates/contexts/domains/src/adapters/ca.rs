//! The sandbox certificate authority: the reference
//! `CertificateAuthorityPort` adapter.
//!
//! Deterministic and key-less: it records issuance facts (serial, dates)
//! exactly as the production adapter would after driving the edge's ACME
//! pipeline. `renewal_error` lets tests (and staging configs) inject a
//! failure mode to exercise the escalation path.

use async_trait::async_trait;
use chrono::{Duration, Utc};
use rand::RngCore;
use uuid::Uuid;

use crate::domain::{Certificate, CertificateAuthorityPort, CustomDomain, DomainsError};

pub struct SandboxCertificateAuthority {
    issuer: String,
    lifetime_days: u32,
    renewal_error: Option<String>,
}

impl SandboxCertificateAuthority {
    pub fn new(lifetime_days: u32) -> Self {
        Self {
            issuer: "Sandbox CA".to_string(),
            lifetime_days,
            renewal_error: None,
        }
    }

    /// Inject a renewal failure mode (tests/staging): every `renew` fails
    /// with this message.
    pub fn with_renewal_error(mut self, detail: impl Into<String>) -> Self {
        self.renewal_error = Some(detail.into());
        self
    }

    fn serial() -> String {
        let mut bytes = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        hex::encode(bytes)
    }
}

#[async_trait]
impl CertificateAuthorityPort for SandboxCertificateAuthority {
    async fn issue(&self, domain: &CustomDomain) -> Result<Certificate, DomainsError> {
        let now = Utc::now();
        Ok(Certificate::issued(
            Uuid::now_v7(),
            domain.id,
            domain.tenant_id,
            domain.hostname.clone(),
            self.issuer.clone(),
            Self::serial(),
            now,
            now + Duration::days(self.lifetime_days as i64),
        ))
    }

    async fn renew(
        &self,
        cert: &Certificate,
        _domain: &CustomDomain,
    ) -> Result<Certificate, DomainsError> {
        if let Some(detail) = &self.renewal_error {
            return Err(DomainsError::CertificateAuthority(detail.clone()));
        }
        let now = Utc::now();
        let mut renewed = cert.clone();
        renewed.renew(now + Duration::days(self.lifetime_days as i64), now);
        renewed.serial = Self::serial();
        Ok(renewed)
    }

    async fn revoke(&self, _cert: &Certificate) -> Result<(), DomainsError> {
        // The sandbox CA keeps no revocation list; the row status flip in
        // the caller is the revocation record.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DomainKind, DomainRole, Hostname};

    fn domain() -> CustomDomain {
        CustomDomain::claim(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Hostname::parse("example.com").unwrap(),
            DomainKind::Connected,
            DomainRole::Primary,
            "token".to_string(),
            Utc::now(),
            72,
            10,
        )
    }

    #[tokio::test]
    async fn issues_with_the_configured_lifetime() {
        let ca = SandboxCertificateAuthority::new(90);
        let cert = ca.issue(&domain()).await.unwrap();
        let days = (cert.not_after - cert.not_before).num_days();
        assert!((89..=90).contains(&days));
    }

    #[tokio::test]
    async fn injected_renewal_failure_escalates() {
        let ca = SandboxCertificateAuthority::new(90).with_renewal_error("simulated outage");
        let cert = ca.issue(&domain()).await.unwrap();
        assert!(matches!(
            ca.renew(&cert, &domain()).await,
            Err(DomainsError::CertificateAuthority(_))
        ));
    }

    #[tokio::test]
    async fn renewal_moves_not_after() {
        let ca = SandboxCertificateAuthority::new(90);
        let cert = ca.issue(&domain()).await.unwrap();
        let renewed = ca.renew(&cert, &domain()).await.unwrap();
        assert!(renewed.not_after > cert.not_after);
        assert!(renewed.renewed_at.is_some());
    }
}
