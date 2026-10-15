//! Observe certificates issued and renewed by the edge's on-demand ACME.
//! Requests retain the customer's SNI but connect only to operator-configured
//! edge IPs. No tenant DNS, redirects, environment proxies or private keys.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sanvi_kernel::RawIdGenerator;

use crate::{
    config::DomainsConfig,
    domain::{Certificate, CertificateAuthorityPort, CertificateId, CustomDomain, DomainsError},
};

pub struct EdgeCertificateAuthority {
    addresses: Vec<SocketAddr>,
    timeout: Duration,
    ids: Arc<dyn RawIdGenerator>,
}

fn ca_error(detail: impl std::fmt::Display) -> DomainsError {
    DomainsError::CertificateAuthority(detail.to_string())
}

fn reconcile_renewal(cert: &Certificate, mut observed: Certificate) -> Certificate {
    if observed.not_after <= cert.not_after {
        return cert.clone();
    }
    observed.renewed_at = Some(Utc::now());
    observed
}

impl EdgeCertificateAuthority {
    pub fn new(config: &DomainsConfig, ids: Arc<dyn RawIdGenerator>) -> Result<Self, DomainsError> {
        let addresses = config
            .anycast_ips
            .iter()
            .map(|ip| {
                let ip = ip.parse().map_err(ca_error)?;
                if !super::prober::is_public(ip) {
                    return Err(ca_error("live TLS requires public edge IP addresses"));
                }
                Ok(SocketAddr::new(ip, 443))
            })
            .collect::<Result<Vec<_>, DomainsError>>()?;
        if addresses.is_empty() || config.cert_request_timeout_secs == 0 {
            return Err(ca_error("live TLS requires edge IPs and a nonzero timeout"));
        }
        Ok(Self {
            addresses,
            timeout: Duration::from_secs(config.cert_request_timeout_secs),
            ids,
        })
    }

    fn client_builder(&self, domain: &CustomDomain) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .timeout(self.timeout)
            .no_proxy()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .tls_info(true)
            .resolve_to_addrs(domain.hostname.as_str(), &self.addresses)
    }

    async fn observe(
        &self,
        domain: &CustomDomain,
        id: CertificateId,
        client: reqwest::Client,
    ) -> Result<Certificate, DomainsError> {
        // The TLS handshake triggers on-demand issuance. HTTP status is not
        // evidence about TLS: even a storefront outage may serve a valid cert.
        let response = client
            .head(format!("https://{}/", domain.hostname))
            .send()
            .await
            .map_err(ca_error)?;
        let der = response
            .extensions()
            .get::<reqwest::tls::TlsInfo>()
            .and_then(|tls| tls.peer_certificate())
            .ok_or_else(|| ca_error("edge did not expose its peer certificate"))?;
        let (_, parsed) = x509_parser::parse_x509_certificate(der).map_err(ca_error)?;
        let validity = parsed.validity();
        let not_before = DateTime::from_timestamp(validity.not_before.timestamp(), 0)
            .ok_or_else(|| ca_error("invalid certificate start time"))?;
        let not_after = DateTime::from_timestamp(validity.not_after.timestamp(), 0)
            .ok_or_else(|| ca_error("invalid certificate expiry time"))?;
        Ok(Certificate::issued(
            id,
            domain.id,
            domain.tenant_id,
            domain.hostname.clone(),
            parsed.issuer().to_string(),
            parsed.raw_serial_as_string(),
            not_before,
            not_after,
        ))
    }
}

#[async_trait]
impl CertificateAuthorityPort for EdgeCertificateAuthority {
    async fn issue(&self, domain: &CustomDomain) -> Result<Certificate, DomainsError> {
        let id = self.ids.next_id().map_err(ca_error)?;
        let client = self.client_builder(domain).build().map_err(ca_error)?;
        self.observe(domain, CertificateId::from_snowflake(id), client)
            .await
    }

    async fn renew(
        &self,
        cert: &Certificate,
        domain: &CustomDomain,
    ) -> Result<Certificate, DomainsError> {
        let client = self.client_builder(domain).build().map_err(ca_error)?;
        let observed = self.observe(domain, cert.id, client).await?;
        Ok(reconcile_renewal(cert, observed))
    }

    async fn revoke(&self, _cert: &Certificate) -> Result<(), DomainsError> {
        // Service withdrawal, not CA-level revocation. Removal deletes the
        // authorization projection; the edge gates every HTTP request as well
        // as issuance, so a cached TLS certificate cannot keep serving a tenant.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CustomDomainId, DomainKind, DomainRole, Hostname};
    use sanvi_kernel::TenantId;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::{TlsAcceptor, rustls};

    fn domain(host: &str) -> CustomDomain {
        CustomDomain::claim(
            CustomDomainId::try_from(1i64).unwrap(),
            TenantId::try_from(1i64).unwrap(),
            Hostname::parse(host).unwrap(),
            DomainKind::Connected,
            DomainRole::Primary,
            "token".to_string(),
            Utc::now(),
            72,
            10,
        )
    }

    async fn edge() -> (EdgeCertificateAuthority, reqwest::Certificate) {
        let cert =
            rcgen::generate_simple_self_signed(vec!["shop.example.com".to_string()]).unwrap();
        let root = reqwest::Certificate::from_der(cert.cert.der()).unwrap();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            if let Ok(mut tls) = acceptor.accept(tcp).await {
                let mut request = [0; 4096];
                let _ = tls.read(&mut request).await;
                // A redirect must not cause the adapter to follow tenant input.
                let _ = tls.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            }
        });
        (
            EdgeCertificateAuthority {
                addresses: vec![address],
                timeout: Duration::from_secs(2),
                ids: sanvi_testsupport::test_ids(),
            },
            root,
        )
    }

    #[tokio::test]
    async fn records_real_certificate_without_following_redirects() {
        let (edge, root) = edge().await;
        let domain = domain("shop.example.com");
        let client = edge
            .client_builder(&domain)
            .add_root_certificate(root)
            .build()
            .unwrap();
        let cert = edge
            .observe(&domain, CertificateId::try_from(42i64).unwrap(), client)
            .await
            .unwrap();
        assert_eq!(cert.id, CertificateId::try_from(42i64).unwrap());
        assert!(cert.not_after > Utc::now());
        assert!(!cert.serial.is_empty());
        assert_ne!(cert.issuer, "Sandbox CA");
    }

    #[tokio::test]
    async fn rejects_untrusted_certificate() {
        let (edge, _) = edge().await;
        assert!(edge.issue(&domain("shop.example.com")).await.is_err());
    }

    #[tokio::test]
    async fn rejects_certificate_for_another_hostname() {
        let (edge, root) = edge().await;
        let domain = domain("other.example.com");
        let client = edge
            .client_builder(&domain)
            .add_root_certificate(root)
            .build()
            .unwrap();
        assert!(
            edge.observe(&domain, CertificateId::try_from(1i64).unwrap(), client)
                .await
                .is_err()
        );
    }

    #[test]
    fn refuses_private_and_placeholder_edge_addresses() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "203.0.113.10",
            "::1",
        ] {
            let config = DomainsConfig {
                anycast_ips: vec![ip.to_string()],
                ..Default::default()
            };
            assert!(EdgeCertificateAuthority::new(&config, sanvi_testsupport::test_ids()).is_err());
        }
    }

    #[test]
    fn observing_unchanged_certificate_preserves_expiry_warnings() {
        let domain = domain("shop.example.com");
        let mut cert = Certificate::issued(
            CertificateId::try_from(1i64).unwrap(),
            domain.id,
            domain.tenant_id,
            domain.hostname.clone(),
            "issuer".to_string(),
            "serial".to_string(),
            Utc::now(),
            Utc::now() + chrono::Duration::days(20),
        );
        cert.expiry_warned_days = Some(30);
        assert_eq!(reconcile_renewal(&cert, cert.clone()), cert);
    }

    #[test]
    fn observing_renewal_records_actual_metadata_and_resets_warnings() {
        let domain = domain("shop.example.com");
        let mut old = Certificate::issued(
            CertificateId::try_from(1i64).unwrap(),
            domain.id,
            domain.tenant_id,
            domain.hostname.clone(),
            "issuer".to_string(),
            "old".to_string(),
            Utc::now(),
            Utc::now() + chrono::Duration::days(20),
        );
        old.expiry_warned_days = Some(30);
        let mut observed = old.clone();
        observed.expiry_warned_days = None;
        observed.serial = "new".to_string();
        observed.not_after += chrono::Duration::days(60);
        let renewed = reconcile_renewal(&old, observed.clone());
        assert_eq!(renewed.not_after, observed.not_after);
        assert_eq!(renewed.serial, "new");
        assert_eq!(renewed.expiry_warned_days, None);
        assert!(renewed.renewed_at.is_some());
    }
}
