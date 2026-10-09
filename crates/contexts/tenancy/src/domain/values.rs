//! Tenant value objects: validated, invariant-enforcing wrappers.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::TenancyError;

/// Words that can never become tenant slugs, regardless of the database
/// blocklist (which operators extend). DNS/phishing surface reduction: these
/// would let a tenant impersonate platform or infrastructure surfaces.
pub const RESERVED_WORDS: &[&str] = &[
    "www",
    "api",
    "admin",
    "mail",
    "mailer",
    "smtp",
    "app",
    "apps",
    "platform",
    "sanvi",
    "root",
    "sys",
    "system",
    "billing",
    "payments",
    "auth",
    "login",
    "signup",
    "register",
    "webhook",
    "webhooks",
    "checkout",
    "account",
    "accounts",
    "status",
    "support",
    "help",
    "docs",
    "blog",
    "shop",
    "store",
    "test",
    "testing",
    "demo",
    "sandbox",
    "staging",
    "stage",
    "prod",
    "production",
    "dev",
    "development",
    "dashboard",
    "manage",
    "portal",
    "cdn",
    "static",
    "assets",
    "metrics",
    "graphql",
];

/// A DNS-safe tenant slug: `[a-z0-9]([a-z0-9-]{1,61}[a-z0-9])?`, at most 63
/// characters, never a reserved word. Immutable after creation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct TenantSlug(String);

impl TenantSlug {
    /// Parse and normalise: input is lowercased, then validated against the
    /// slug grammar and the reserved-word blocklist.
    pub fn parse(input: &str) -> Result<Self, TenancyError> {
        let slug = input.to_ascii_lowercase();
        Self::validate(&slug)?;
        Ok(Self(slug))
    }

    fn validate(slug: &str) -> Result<(), TenancyError> {
        if slug.is_empty() || slug.len() > 63 {
            return Err(TenancyError::InvalidSlug(slug.to_string()));
        }

        // Grammar (matches the phase-01 plan regex
        // `[a-z0-9]([a-z0-9-]{1,61}[a-z0-9])?`): one leading alphanumeric,
        // then an optional group of `{1,61}` middle chars (alphanumeric or
        // hyphen) closed by an alphanumeric — so slugs have length 1 or >= 3,
        // never start or end with a hyphen.
        let mut chars = slug.chars();
        let Some(first) = chars.next() else {
            return Err(TenancyError::InvalidSlug(slug.to_string()));
        };
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(TenancyError::InvalidSlug(slug.to_string()));
        }

        let rest: Vec<char> = chars.collect();
        if !rest.is_empty() {
            if rest.len() < 2 {
                return Err(TenancyError::InvalidSlug(slug.to_string()));
            }
            let (middle, last) = rest.split_at(rest.len() - 1);
            if middle.len() > 61
                || middle
                    .iter()
                    .any(|c| !c.is_ascii_lowercase() && !c.is_ascii_digit() && *c != '-')
                || (!last[0].is_ascii_lowercase() && !last[0].is_ascii_digit())
            {
                return Err(TenancyError::InvalidSlug(slug.to_string()));
            }
        }

        if RESERVED_WORDS.contains(&slug) {
            return Err(TenancyError::SlugReserved {
                slug: slug.to_string(),
            });
        }
        Ok(())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantSlug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for TenantSlug {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Human-facing tenant name (1–100 chars, trimmed, non-empty).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TenantName(String);

impl TenantName {
    pub fn parse(input: &str) -> Result<Self, TenancyError> {
        let name = input.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(TenancyError::InvalidDisplayName(name.to_string()));
        }
        Ok(Self(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TenantName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for TenantName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Where a tenant's data is stored. Recorded from day one so residency and
/// transfer rules (phase 05) never need a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DataRegion {
    Eu,
    Jp,
    Us,
}

impl DataRegion {
    pub fn parse(input: &str) -> Result<Self, TenancyError> {
        match input.to_ascii_lowercase().as_str() {
            "eu" => Ok(Self::Eu),
            "jp" => Ok(Self::Jp),
            "us" => Ok(Self::Us),
            other => Err(TenancyError::InvalidRegion(other.to_string())),
        }
    }
}

impl fmt::Display for DataRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Eu => write!(f, "eu"),
            Self::Jp => write!(f, "jp"),
            Self::Us => write!(f, "us"),
        }
    }
}

/// BCP 47-ish locale tag (e.g. `en`, `ja`, `en-GB`). Phase 06 consumes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[schema(as = TenancyLocale)]
pub struct Locale(String);

impl Locale {
    pub const DEFAULT: &'static str = "en";

    pub fn parse(input: &str) -> Result<Self, TenancyError> {
        if input.len() < 2
            || input.len() > 35
            || !input.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            || input.starts_with('-')
            || input.ends_with('-')
        {
            return Err(TenancyError::InvalidLocale(input.to_string()));
        }
        Ok(Self(input.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Locale {
    fn default() -> Self {
        Self(Self::DEFAULT.to_string())
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for Locale {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Why a tenant is suspended — machine-readable in the 423 problem body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SuspensionReason {
    Billing,
    Abuse,
    Legal,
    Operational,
}

impl SuspensionReason {
    pub fn parse(input: &str) -> Result<Self, TenancyError> {
        match input {
            "billing" => Ok(Self::Billing),
            "abuse" => Ok(Self::Abuse),
            "legal" => Ok(Self::Legal),
            "operational" => Ok(Self::Operational),
            other => Err(TenancyError::InvalidSuspensionReason(other.to_string())),
        }
    }
}

impl fmt::Display for SuspensionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Billing => write!(f, "billing"),
            Self::Abuse => write!(f, "abuse"),
            Self::Legal => write!(f, "legal"),
            Self::Operational => write!(f, "operational"),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn parses_valid_slugs_and_normalises_case() {
        assert_eq!(TenantSlug::parse("Acme").unwrap().as_str(), "acme");
        assert_eq!(TenantSlug::parse("a").unwrap().as_str(), "a");
        assert_eq!(
            TenantSlug::parse("acme-corp").unwrap().as_str(),
            "acme-corp"
        );
        assert_eq!(TenantSlug::parse("acme--x").unwrap().as_str(), "acme--x");
        assert!(TenantSlug::parse("a1").is_err());
        let long = "a".repeat(63);
        assert!(TenantSlug::parse(&long).is_ok());
    }

    #[test]
    fn rejects_invalid_slugs() {
        for bad in [
            "", "-acme", "acme-", "acme..x", "acme_x", "acmé", "ab", "a-", "-a", "a a", "Acme!",
        ] {
            assert!(
                TenantSlug::parse(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(TenantSlug::parse(&"a".repeat(64)).is_err());
    }

    #[test]
    fn rejects_reserved_words() {
        for word in ["www", "api", "admin", "mail", "platform", "billing"] {
            assert!(matches!(
                TenantSlug::parse(word),
                Err(TenancyError::SlugReserved { .. })
            ));
        }
    }

    #[test]
    fn slug_is_immutable_value_semantics() {
        let a = TenantSlug::parse("acme").unwrap();
        let b = TenantSlug::parse("ACME").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "acme");
    }

    proptest! {
        #[test]
        fn slug_parsing_is_idempotent_and_never_panics(raw in "\\PC{0,80}") {
            let _ = TenantSlug::parse(&raw);
            if let Ok(slug) = TenantSlug::parse(&raw) {
                prop_assert_eq!(slug.as_str(), slug.as_str().to_ascii_lowercase());
                prop_assert_eq!(TenantSlug::parse(slug.as_str()).unwrap(), slug);
            }
        }
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(
            TenantName::parse("  Acme Corp  ").unwrap().as_str(),
            "Acme Corp"
        );
        assert!(TenantName::parse("   ").is_err());
        assert!(TenantName::parse(&"x".repeat(101)).is_err());
        assert!(TenantName::parse(&"x".repeat(100)).is_ok());
    }

    #[test]
    fn region_parsing_is_case_insensitive() {
        assert_eq!(DataRegion::parse("EU").unwrap(), DataRegion::Eu);
        assert_eq!(DataRegion::parse("jp").unwrap(), DataRegion::Jp);
        assert_eq!(DataRegion::parse("us").unwrap(), DataRegion::Us);
        assert!(DataRegion::parse("cn").is_err());
    }

    #[test]
    fn locale_parsing() {
        assert_eq!(Locale::parse("en").unwrap().as_str(), "en");
        assert_eq!(Locale::parse("ja-JP").unwrap().as_str(), "ja-JP");
        assert!(Locale::parse("e").is_err());
        assert!(Locale::parse("-en").is_err());
        assert!(Locale::parse("en-").is_err());
        assert_eq!(Locale::default().as_str(), "en");
    }

    #[test]
    fn suspension_reason_roundtrips() {
        assert_eq!(
            SuspensionReason::parse("billing").unwrap(),
            SuspensionReason::Billing
        );
        assert_eq!(
            serde_json::to_string(&SuspensionReason::Abuse).unwrap(),
            "\"abuse\""
        );
        assert!(SuspensionReason::parse("nope").is_err());
    }
}
