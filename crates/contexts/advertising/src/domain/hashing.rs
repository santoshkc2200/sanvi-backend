//! Identifier hashing for conversion capture (TASK-014). Normalise, then
//! SHA-256 — raw PII is never constructible outside these two functions,
//! nor stored anywhere in this context.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(transparent)]
pub struct HashedIdentifier(String);

impl HashedIdentifier {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn sha256_hex(input: &str) -> HashedIdentifier {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    HashedIdentifier(format!("{:064x}", hasher.finalize()))
}

/// Trim and lowercase; then, for Gmail addresses only, also strip dots and
/// a `+tag` from the local part.
///
/// That extra step is Gmail-specific: Google's enhanced-conversion spec
/// defines it for `gmail.com`/`googlemail.com` alone, and Meta strips
/// neither dots nor tags for any domain. Applying it everywhere hashed
/// `first.last@company.com` as `firstlast@company.com`, an address that
/// matches nothing on either platform — and since only the digest is
/// persisted, those conversions could never be repaired after the fact.
pub fn hash_email(raw_email: &str) -> HashedIdentifier {
    let email = raw_email.trim().to_lowercase();
    let normalized = match email.split_once('@') {
        Some((local, domain @ ("gmail.com" | "googlemail.com"))) => {
            let mut local = local.replace('.', "");
            if let Some(plus_at) = local.find('+') {
                local.truncate(plus_at);
            }
            format!("{local}@{domain}")
        }
        _ => email,
    };
    sha256_hex(&normalized)
}

/// E.164-style normalization before hashing: a leading `+` plus digits
/// only, everything else (spaces, dashes, parens) stripped.
pub fn hash_phone(raw_phone: &str) -> HashedIdentifier {
    let trimmed = raw_phone.trim();
    let mut normalized = String::with_capacity(trimmed.len());
    for (index, character) in trimmed.chars().enumerate() {
        let keep = character.is_ascii_digit() || (index == 0 && character == '+');
        if keep {
            normalized.push(character);
        }
    }
    sha256_hex(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmail_normalization_ignores_case_dots_and_tags() {
        assert_eq!(
            hash_email(" A.User+tag@GMail.COM "),
            hash_email("auser@gmail.com")
        );
        assert_eq!(
            hash_email("a.user@googlemail.com"),
            hash_email("auser@googlemail.com")
        );
    }

    /// Only Gmail collapses dots and tags. Doing it elsewhere produces a
    /// digest no ad platform will ever match.
    #[test]
    fn other_domains_keep_dots_and_tags_and_only_lose_case() {
        assert_ne!(
            hash_email("first.last@company.com"),
            hash_email("firstlast@company.com")
        );
        assert_ne!(
            hash_email("user+tag@company.com"),
            hash_email("user@company.com")
        );
        assert_eq!(
            hash_email(" First.Last@Company.COM "),
            hash_email("first.last@company.com")
        );
    }

    #[test]
    fn phone_normalization_strips_formatting() {
        assert_eq!(hash_phone("+1 (415) 555-0100"), hash_phone("+14155550100"));
    }

    #[test]
    fn distinct_inputs_hash_differently() {
        assert_ne!(hash_email("a@example.com"), hash_email("b@example.com"));
    }
}
