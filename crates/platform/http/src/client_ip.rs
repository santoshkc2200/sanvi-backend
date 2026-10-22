//! Security-sensitive client identity. Forwarding claims require an explicitly
//! trusted immediate peer and exactly one address sanitized by that proxy.

use actix_web::HttpRequest;
use std::net::IpAddr;

#[derive(Clone, Debug, Default)]
pub struct TrustedProxies(Vec<(IpAddr, u8)>);

impl TrustedProxies {
    pub fn parse(entries: &[String]) -> anyhow::Result<Self> {
        let mut networks = Vec::new();
        for entry in entries {
            let (address, prefix) = match entry.split_once('/') {
                Some((address, prefix)) => {
                    (address.parse::<IpAddr>()?, Some(prefix.parse::<u8>()?))
                }
                None => (entry.parse::<IpAddr>()?, None),
            };
            let bits = if address.is_ipv4() { 32 } else { 128 };
            let prefix = prefix.unwrap_or(bits);
            anyhow::ensure!(prefix <= bits, "invalid trusted proxy CIDR: {entry}");
            networks.push((address, prefix));
        }
        Ok(Self(networks))
    }

    fn contains(&self, peer: IpAddr) -> bool {
        self.0
            .iter()
            .any(|(network, prefix)| match (*network, peer) {
                (IpAddr::V4(a), IpAddr::V4(b)) => {
                    let mask = u32::MAX.checked_shl(32 - u32::from(*prefix)).unwrap_or(0);
                    u32::from(a) & mask == u32::from(b) & mask
                }
                (IpAddr::V6(a), IpAddr::V6(b)) => {
                    let mask = u128::MAX.checked_shl(128 - u32::from(*prefix)).unwrap_or(0);
                    u128::from(a) & mask == u128::from(b) & mask
                }
                _ => false,
            })
    }
}

pub fn client_ip(req: &HttpRequest) -> Option<IpAddr> {
    let peer = req.peer_addr()?.ip();
    let policy = req.app_data::<TrustedProxies>();
    if !policy.is_some_and(|policy| policy.contains(peer)) {
        return Some(peer);
    }
    let mut values = req.headers().get_all("x-forwarded-for");
    let address = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());
    if values.next().is_some() {
        return Some(peer);
    }
    address.or(Some(peer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test::TestRequest;

    #[test]
    fn untrusted_forwarding_headers_cannot_change_client_identity() {
        let req = TestRequest::default()
            .peer_addr("192.0.2.5:9000".parse().unwrap())
            .insert_header(("forwarded", "for=198.51.100.1"))
            .insert_header(("x-forwarded-for", "198.51.100.2"))
            .to_http_request();
        assert_eq!(client_ip(&req), Some("192.0.2.5".parse().unwrap()));
    }

    #[test]
    fn only_sanitized_single_ip_from_a_trusted_peer_is_accepted() {
        for (header, expected) in [
            ("198.51.100.2", "198.51.100.2"),
            ("198.51.100.2, 198.51.100.3", "192.0.2.5"),
            ("invalid", "192.0.2.5"),
        ] {
            let req = TestRequest::default()
                .peer_addr("192.0.2.5:9000".parse().unwrap())
                .app_data(TrustedProxies::parse(&["192.0.2.5/32".into()]).unwrap())
                .insert_header(("forwarded", "for=203.0.113.99"))
                .insert_header(("x-forwarded-for", header))
                .to_http_request();
            assert_eq!(client_ip(&req), Some(expected.parse().unwrap()));
        }
    }

    #[test]
    fn cidr_validation_and_ipv6_trust_are_explicit() {
        assert!(TrustedProxies::parse(&["10.0.0.1/33".into()]).is_err());
        let policy = TrustedProxies::parse(&["2001:db8::/64".into()]).unwrap();
        assert!(policy.contains("2001:db8::1".parse().unwrap()));
        assert!(!policy.contains("2001:db9::1".parse().unwrap()));
    }
}
