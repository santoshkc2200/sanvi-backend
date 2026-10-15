//! The storefront prober: HTTP(S) reachability with an SSRF guard.
//!
//! Before the request fires, the hostname resolves and every address is
//! checked against private/loopback/link-local/reserved ranges. Redirects
//! are limited and each hop re-checks its own resolved addresses, so a
//! tenant-supplied hostname cannot steer the probe into internal ranges.

use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::lookup_host;

use crate::domain::{DomainsError, Hostname, ProbeResult, StorefrontProbePort};

const MAX_REDIRECTS: u32 = 3;

pub struct HttpProbeAdapter {
    client: reqwest::Client,
    timeout: Duration,
}

impl HttpProbeAdapter {
    pub fn new(timeout_secs: u64, user_agent: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static reqwest client construction is infallible");
        Self {
            client,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// Every resolved address must be globally routable.
    async fn assert_public(host: &str) -> Result<(), DomainsError> {
        let addresses: Vec<IpAddr> = lookup_host((host, 443))
            .await
            .map_err(|err| DomainsError::Probe(format!("dns resolution failed: {err}")))?
            .map(|address| address.ip())
            .collect();
        if addresses.is_empty() {
            return Err(DomainsError::Probe("no addresses resolved".to_string()));
        }
        for address in addresses {
            if !is_public(address) {
                return Err(DomainsError::Probe(format!(
                    "refusing to probe non-public address {address}"
                )));
            }
        }
        Ok(())
    }
}

/// The SSRF allow-list: only globally routable unicast.
pub(super) fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                // 100.64.0.0/10 shared address space (CGNAT).
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 0b0100_0000)
                // 224.0.0.0/4 multicast.
                || (v4.octets()[0] & 0b1111_0000) == 0b1110_0000
                || v4.octets()[0] >= 240)
        }
        IpAddr::V6(v6) => {
            // An IPv4-mapped address carries an IPv4 literal: judge it as
            // one, so `::ffff:10.0.0.1` is refused exactly like `10.0.0.1`.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_multicast()
                // 2001:db8::/32 documentation.
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0x0db8)
                // 2001::/32 Teredo tunnels an inner address we cannot see.
                || (v6.segments()[0] == 0x2001 && v6.segments()[1] == 0)
                // 2002::/16 6to4 likewise embeds an arbitrary IPv4 target.
                || v6.segments()[0] == 0x2002
                || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

#[async_trait]
impl StorefrontProbePort for HttpProbeAdapter {
    async fn probe(&self, hostname: &Hostname) -> Result<ProbeResult, DomainsError> {
        let mut url = format!("https://{hostname}/");
        let mut redirects = 0;
        loop {
            // Parse the current URL's host and re-check its addresses
            // (redirect targets are tenant-controlled too).
            let parsed = reqwest::Url::parse(&url)
                .map_err(|err| DomainsError::Probe(format!("invalid url {url}: {err}")))?;
            let host = parsed
                .host_str()
                .ok_or_else(|| DomainsError::Probe("url has no host".to_string()))?;
            Self::assert_public(host).await?;

            let response = self
                .client
                .get(url.clone())
                .timeout(self.timeout)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    return Ok(ProbeResult {
                        ok: false,
                        status: None,
                        redirects,
                        detail: format!("request failed: {err}"),
                    });
                }
            };

            let status = response.status();
            if status.is_redirection() {
                if redirects >= MAX_REDIRECTS {
                    return Ok(ProbeResult {
                        ok: false,
                        status: Some(status.as_u16()),
                        redirects,
                        detail: format!("too many redirects ({MAX_REDIRECTS})"),
                    });
                }
                let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
                    return Ok(ProbeResult {
                        ok: false,
                        status: Some(status.as_u16()),
                        redirects,
                        detail: "redirect without a Location header".to_string(),
                    });
                };
                let location = location.to_str().unwrap_or_default().to_string();
                url = reqwest::Url::parse(&url)
                    .and_then(|base| base.join(&location))
                    .map_err(|err| DomainsError::Probe(format!("bad redirect target: {err}")))?
                    .to_string();
                redirects += 1;
                continue;
            }
            return Ok(ProbeResult {
                ok: status.is_success(),
                status: Some(status.as_u16()),
                redirects,
                detail: String::new(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses_pass() {
        assert!(is_public("93.184.216.34".parse().unwrap()));
        assert!(is_public("8.8.8.8".parse().unwrap()));
        // Real public IPv6 space (the old mask wrongly refused 2000::/9).
        assert!(is_public("2001:4860:4860::8888".parse().unwrap()));
        assert!(is_public("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn private_and_special_ranges_are_refused() {
        for blocked in [
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            let address: IpAddr = blocked.parse().unwrap();
            assert!(!is_public(address), "{blocked} must be refused");
        }
    }

    #[test]
    fn ipv6_special_ranges_are_refused() {
        assert!(!is_public("::1".parse().unwrap()));
        assert!(!is_public("fe80::1".parse().unwrap()));
        assert!(!is_public("fc00::1".parse().unwrap()));
        assert!(!is_public("2001:db8::1".parse().unwrap()));
        // Mapped, Teredo and 6to4 embed addresses we cannot inspect.
        assert!(!is_public("::ffff:10.0.0.1".parse().unwrap()));
        assert!(!is_public("2001::10.0.0.1".parse().unwrap()));
        assert!(!is_public("2002:0a00:0001::1".parse().unwrap()));
    }
}
