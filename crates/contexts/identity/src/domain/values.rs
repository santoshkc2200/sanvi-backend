//! Value objects: validated, invariant-enforcing wrappers.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::IdentityError;

/// A normalized email address: trimmed and lowercased at the boundary so
/// every comparison, index and unique constraint in the system sees one
/// canonical form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct Email(String);

impl Email {
    /// Parse and normalize. Enforces a pragmatic shape: one `@`, non-empty
    /// local part and a dotted domain. Full RFC 5322 validation is Kratos'
    /// job; this guards storage invariants.
    pub fn parse(input: &str) -> Result<Self, IdentityError> {
        let email = input.trim().to_ascii_lowercase();
        let (local, domain) = email
            .split_once('@')
            .ok_or_else(|| IdentityError::InvalidEmail(input.to_string()))?;
        if local.is_empty()
            || domain.is_empty()
            || !domain.contains('.')
            || email.len() > 254
            || email.chars().any(char::is_whitespace)
        {
            return Err(IdentityError::InvalidEmail(input.to_string()));
        }
        Ok(Self(email))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Email {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Email> for String {
    fn from(email: Email) -> Self {
        email.0
    }
}

/// An OIDC/SSO provider id, e.g. `google` or `microsoft`. Lowercase
/// `[a-z0-9-]`, because it names a configuration entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn parse(input: &str) -> Result<Self, IdentityError> {
        let id = input.trim().to_ascii_lowercase();
        if id.is_empty()
            || id.len() > 63
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(IdentityError::InvalidProvider(input.to_string()));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The provider-side identifier of an account (`sub` claim). Opaque to us:
/// stored as-is, compared exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderSubject(String);

impl ProviderSubject {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A BCP-47 language tag. The full catalogue lives in the localization
/// context (phase 06); here we only enforce well-formedness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct Locale(String);

impl Locale {
    pub const DEFAULT: &'static str = "en";

    pub fn parse(input: &str) -> Result<Self, IdentityError> {
        let tag = input.trim().to_ascii_lowercase();
        let valid = tag.len() >= 2
            && tag.len() <= 12
            && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && tag.split('-').all(|part| {
                !part.is_empty() && part.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            });
        if !valid {
            return Err(IdentityError::InvalidLocale(input.to_string()));
        }
        Ok(Self(tag))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_is_lowercased_and_trimmed() {
        assert_eq!(
            Email::parse("  Alice@Example.COM ").unwrap().as_str(),
            "alice@example.com"
        );
    }

    #[test]
    fn email_rejects_malformed_shapes() {
        let too_long = format!("{}@example.com", "a".repeat(250));
        for bad in [
            "",
            "no-at-sign",
            "@nope.com",
            "a@b",
            "a b@c.com",
            too_long.as_str(),
        ] {
            assert!(
                matches!(Email::parse(bad), Err(IdentityError::InvalidEmail(_))),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn provider_id_rejects_unexpected_shapes() {
        assert!(matches!(
            ProviderId::parse("Google!"),
            Err(IdentityError::InvalidProvider(_))
        ));
        assert!(matches!(
            ProviderId::parse(""),
            Err(IdentityError::InvalidProvider(_))
        ));
        assert_eq!(ProviderId::parse("Google").unwrap().as_str(), "google");
    }

    #[test]
    fn locale_rejects_unexpected_shapes() {
        assert!(matches!(
            Locale::parse(""),
            Err(IdentityError::InvalidLocale(_))
        ));
        assert!(matches!(
            Locale::parse("1"),
            Err(IdentityError::InvalidLocale(_))
        ));
        assert_eq!(Locale::parse("en-US").unwrap().as_str(), "en-us");
    }
}
