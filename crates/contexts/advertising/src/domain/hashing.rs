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

/// Lowercase, trim, and strip dots and a `+tag` from the local part before
/// hashing — the enhanced-match normalization every ad platform's upload
/// API expects, so `A.User+tag@Example.COM` and `auser@example.com` hash
/// identically.
pub fn hash_email(raw_email: &str) -> HashedIdentifier {
    let email = raw_email.trim().to_lowercase();
    let normalized = match email.split_once('@') {
        Some((local, domain)) => {
            let mut local = local.replace('.', "");
            if let Some(plus_at) = local.find('+') {
                local.truncate(plus_at);
            }
            format!("{local}@{domain}")
        }
        None => email,
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
    fn email_normalization_is_case_and_tag_insensitive() {
        assert_eq!(
            hash_email("A.User+tag@Example.COM"),
            hash_email("auser@example.com")
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
