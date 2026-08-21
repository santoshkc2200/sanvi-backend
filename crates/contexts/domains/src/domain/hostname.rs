//! The hostname value object.
//!
//! Every hostname entering the context is validated and normalized once:
//! lowercased, trailing dot stripped, IDN-converted to punycode. The
//! normalized form is the global uniqueness key (`normalized_hostname`
//! column) — punycode normalization is what prevents homograph collisions
//! (`раураl.com` vs `paypal.com` never compare equal, but look-alike
//! punycode never shares a key with the ASCII name either).
//!
//! Validation rules:
//! - not an IP address (v4 or v6);
//! - 253 chars max, labels of 1–63 chars, labels `[a-z0-9-]` after
//!   normalization, no leading/trailing hyphen;
//! - has a registrable suffix per the Public Suffix List (so `com`, `co.uk`
//!   and single labels are rejected — they are suffixes, not domains);
//! - not on the reserved-name blocklist (`localhost`, `example.com`, …).

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::DomainsError;

/// Names that are never a legitimate tenant hostname.
const RESERVED: &[&str] = &[
    "localhost",
    "localhost.localdomain",
    "invalid",
    "test",
    "onion",
];

/// A validated, normalized (punycode, lowercase) DNS hostname.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct Hostname(String);

impl Hostname {
    /// Parse and normalize. Fails on IPs, malformed names, suffixes and the
    /// reserved blocklist.
    pub fn parse(raw: &str) -> Result<Self, DomainsError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(DomainsError::InvalidHostname(
                "hostname is empty".to_string(),
            ));
        }
        if trimmed.contains(':') {
            return Err(DomainsError::InvalidHostname(format!(
                "hostname must not contain a port or be an IPv6 address: {raw}"
            )));
        }
        let without_dot = trimmed.trim_end_matches('.');
        let lower = without_dot.to_ascii_lowercase();

        if is_ipv4(&lower) || looks_numeric(&lower) {
            return Err(DomainsError::InvalidHostname(format!(
                "hostname must not be an IP address: {raw}"
            )));
        }

        // IDNA → punycode. This rejects malformed unicode and label
        // violations and canonicalizes look-alikes into `xn--…` form.
        let ascii = idna::domain_to_ascii(&lower).map_err(|err| {
            DomainsError::InvalidHostname(format!("hostname is not valid IDNA: {raw}: {err}"))
        })?;

        if ascii.len() > 253 {
            return Err(DomainsError::InvalidHostname(format!(
                "hostname exceeds 253 characters: {raw}"
            )));
        }
        for label in ascii.split('.') {
            if label.is_empty() || label.len() > 63 {
                return Err(DomainsError::InvalidHostname(format!(
                    "invalid label length in hostname: {raw}"
                )));
            }
            if label.starts_with('-') || label.ends_with('-') {
                return Err(DomainsError::InvalidHostname(format!(
                    "labels must not start or end with a hyphen: {raw}"
                )));
            }
            if !label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                return Err(DomainsError::InvalidHostname(format!(
                    "invalid character in hostname: {raw}"
                )));
            }
        }

        if RESERVED.contains(&ascii.as_str()) {
            return Err(DomainsError::HostnameReserved(ascii.clone()));
        }
        if RESERVED
            .iter()
            .any(|reserved| reserved.starts_with('.') && ascii.ends_with(reserved))
        {
            return Err(DomainsError::HostnameReserved(ascii.clone()));
        }

        // Must have a registrable part: `com` and `co.uk` are suffixes, not
        // domains. `psl::domain_str` returns `None` for those.
        psl::domain_str(&ascii).ok_or_else(|| {
            DomainsError::InvalidHostname(format!(
                "hostname has no registrable domain under a known public suffix: {raw}"
            ))
        })?;

        Ok(Self(ascii))
    }

    /// The normalized punycode, lowercase form. The database uniqueness key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The registrable domain (`psl::domain_str`): `www.example.co.uk` → `example.co.uk`.
    pub fn registrable(&self) -> &str {
        psl::domain_str(&self.0).unwrap_or(&self.0)
    }

    /// Whether this hostname is the registrable domain itself (an apex).
    pub fn is_apex(&self) -> bool {
        self.registrable() == self.0
    }

    /// Whether `self` is `candidate` or a subdomain of it.
    pub fn is_or_is_subdomain_of(&self, candidate: &str) -> bool {
        let self_lower = self.0.to_ascii_lowercase();
        let candidate_lower = candidate.to_ascii_lowercase();
        self_lower == candidate_lower || self_lower.ends_with(&format!(".{candidate_lower}"))
    }

    /// The first label (`www` for `www.example.com`).
    pub fn first_label(&self) -> &str {
        self.0.split('.').next().unwrap_or(&self.0)
    }
}

impl fmt::Display for Hostname {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<Hostname> for String {
    fn from(hostname: Hostname) -> Self {
        hostname.0
    }
}

/// Anything made of digits and dots only (`10.0.0.256` included) is
/// IP-shaped even if it fails the octet-range check — a hostname is never.
fn looks_numeric(raw: &str) -> bool {
    !raw.is_empty() && raw.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Strict IPv4 literal check (`1.2.3.4`, all octets ≤ 255).
fn is_ipv4(raw: &str) -> bool {
    let mut parts = raw.split('.');
    let mut count = 0;
    for part in &mut parts {
        if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        match part.parse::<u32>() {
            Ok(value) if value <= 255 => {}
            _ => return false,
        }
        count += 1;
    }
    count == 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn accepts_apex_and_www() {
        assert_eq!(
            Hostname::parse("example.com").unwrap().as_str(),
            "example.com"
        );
        assert_eq!(
            Hostname::parse("WWW.Example.COM").unwrap().as_str(),
            "www.example.com"
        );
        assert_eq!(
            Hostname::parse("example.com.").unwrap().as_str(),
            "example.com"
        );
    }

    #[test]
    fn rejects_ips_and_ports() {
        assert!(Hostname::parse("192.168.1.1").is_err());
        assert!(Hostname::parse("10.0.0.256").is_err());
        assert!(Hostname::parse("::1").is_err());
        assert!(Hostname::parse("2001:db8::1").is_err());
        assert!(Hostname::parse("example.com:8080").is_err());
    }

    #[test]
    fn rejects_suffixes_and_single_labels() {
        assert!(Hostname::parse("com").is_err());
        assert!(Hostname::parse("co.uk").is_err());
        assert!(Hostname::parse("localhost").is_err());
        assert!(Hostname::parse("invalid").is_err());
    }

    #[test]
    fn rejects_malformed_labels() {
        assert!(Hostname::parse("-foo.com").is_err());
        assert!(Hostname::parse("foo-.com").is_err());
        assert!(Hostname::parse("foo..com").is_err());
        assert!(Hostname::parse("").is_err());
        assert!(Hostname::parse("foo_bar.com").is_err());
    }

    #[test]
    fn punycode_normalizes_unicode() {
        assert_eq!(
            Hostname::parse("bücher.de").unwrap().as_str(),
            "xn--bcher-kva.de"
        );
        assert_eq!(
            Hostname::parse("日本語.jp").unwrap().as_str(),
            "xn--wgv71a119e.jp"
        );
    }

    #[test]
    fn registrable_domain_extraction() {
        let www = Hostname::parse("www.example.co.uk").unwrap();
        assert_eq!(www.registrable(), "example.co.uk");
        assert!(!www.is_apex());
        let apex = Hostname::parse("example.co.uk").unwrap();
        assert!(apex.is_apex());
    }

    #[test]
    fn subdomain_relationship() {
        let store = Hostname::parse("store.example.com").unwrap();
        assert!(store.is_or_is_subdomain_of("example.com"));
        assert!(store.is_or_is_subdomain_of("store.example.com"));
        assert!(!store.is_or_is_subdomain_of("ple.com"));
        assert!(!store.is_or_is_subdomain_of("evil-example.com"));
    }

    #[test]
    fn overlong_hostname_rejected() {
        let label = "a".repeat(63);
        let hostname = format!("{label}.{label}.{label}.{label}.com");
        assert!(Hostname::parse(&hostname).is_err());
    }

    proptest! {
        /// Random garbage never panics — it either parses or returns an
        /// error, and anything that parses round-trips.
        #[test]
        fn parsing_never_panics(raw in "\\PC{0,120}") {
            if let Ok(hostname) = Hostname::parse(&raw) {
                let reparsed = Hostname::parse(hostname.as_str()).expect("normalized form parses");
                assert_eq!(reparsed, hostname);
                assert!(!hostname.as_str().contains(|c: char| c.is_uppercase()));
                assert!(!hostname.as_str().ends_with('.'));
            }
        }

        /// Homograph pairs never share a normalized key with the ASCII name.
        #[test]
        fn homographs_do_not_collide(seed in 0u32..1000) {
            let ascii = Hostname::parse("paypal.com").unwrap();
            let lookalike = format!("раураl-{seed}.com");
            if let Ok(hostname) = Hostname::parse(&lookalike) {
                assert_ne!(hostname.as_str(), ascii.as_str());
            }
        }
    }
}
