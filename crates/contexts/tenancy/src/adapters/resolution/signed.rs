//! Signed internal tenant tokens (`x-sanvi-tenant`).
//!
//! Service-to-service and worker requests carry a tenant id the caller
//! asserts. Trust is established with a rotating HMAC key: the token binds
//! key id + tenant id + timestamp under HMAC-SHA256, and verification
//! enforces clock-skew tolerance. An invalid token is rejected loudly — it
//! is never silently ignored, which would let a caller strip the header and
//! fall through to subdomain resolution.

use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

const VERSION: &str = "v1";

/// A signing key: id plus secret. Newest key first; verification accepts any
/// key in the list, minting always uses the first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SigningKey {
    pub id: String,
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignedTokenError {
    #[error("malformed internal tenant token")]
    Malformed,
    #[error("internal tenant token signed by unknown key {0}")]
    UnknownKey(String),
    #[error("internal tenant token signature mismatch")]
    BadSignature,
    #[error("internal tenant token outside clock-skew tolerance")]
    Expired,
}

type HmacSha256 = Hmac<Sha256>;

/// Mint a token asserting `tenant_id` (uses key `k1`).
pub fn mint(key: &SigningKey, tenant_id: Uuid, now: DateTime<Utc>) -> String {
    let ts = now.timestamp();
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tenant_id.as_bytes());
    let mac_input = format!("{VERSION}.{}.{b64}.{ts}", key.id);
    let mut mac =
        HmacSha256::new_from_slice(key.secret.as_bytes()).expect("hmac accepts keys of any length");
    mac.update(mac_input.as_bytes());
    let signature = hex_encode(&mac.finalize().into_bytes());
    format!("{mac_input}.{signature}")
}

/// Verify a token against the active key set, enforcing skew tolerance.
pub fn verify(
    token: &str,
    keys: &[SigningKey],
    now: DateTime<Utc>,
    skew: Duration,
) -> Result<Uuid, SignedTokenError> {
    let parts: Vec<&str> = token.split('.').collect();
    let [version, key_id, b64, ts_raw, signature] = parts.as_slice() else {
        return Err(SignedTokenError::Malformed);
    };
    if *version != VERSION {
        return Err(SignedTokenError::Malformed);
    }

    let key = keys
        .iter()
        .find(|key| key.id == *key_id)
        .ok_or_else(|| SignedTokenError::UnknownKey((*key_id).to_string()))?;

    let ts: i64 = ts_raw.parse().map_err(|_| SignedTokenError::Malformed)?;
    let token_time = DateTime::<Utc>::from_timestamp(ts, 0).ok_or(SignedTokenError::Malformed)?;
    let age = (now - token_time).num_seconds().unsigned_abs();
    let skew_secs = skew.as_secs();
    // Accept tokens from a small future window too (clock drift between
    // hosts), bounded by the same skew.
    if age > skew_secs && (token_time - now).num_seconds().unsigned_abs() > skew_secs {
        return Err(SignedTokenError::Expired);
    }

    let mac_input = format!("{VERSION}.{key_id}.{b64}.{ts}");
    let mut mac =
        HmacSha256::new_from_slice(key.secret.as_bytes()).expect("hmac accepts keys of any length");
    mac.update(mac_input.as_bytes());
    // Constant-time comparison (HMAC verify).
    mac.verify_slice(&hex_decode(signature).map_err(|_| SignedTokenError::Malformed)?)
        .map_err(|_| SignedTokenError::BadSignature)?;

    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(b64)
        .map_err(|_| SignedTokenError::Malformed)?;
    Uuid::from_slice(&bytes).map_err(|_| SignedTokenError::Malformed)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(input: &str) -> Result<Vec<u8>, ()> {
    if !input.len().is_multiple_of(2) {
        return Err(());
    }
    (0..input.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&input[index..index + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey {
            id: "k1".to_string(),
            secret: "secret-material".to_string(),
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-16T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn mint_and_verify_roundtrip() {
        let tenant = Uuid::now_v7();
        let token = mint(&key(), tenant, now());
        assert_eq!(
            verify(&token, &[key()], now(), Duration::from_secs(60)).unwrap(),
            tenant
        );
    }

    #[test]
    fn tampered_payload_fails() {
        let tenant = Uuid::now_v7();
        let token = mint(&key(), tenant, now());
        let mut tampered = token.as_bytes().to_vec();
        // Flip one byte of the base64 tenant id segment.
        let pos = "v1.k1.".len();
        tampered[pos] ^= 1;
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(matches!(
            verify(&tampered, &[key()], now(), Duration::from_secs(60)),
            Err(SignedTokenError::BadSignature) | Err(SignedTokenError::Malformed)
        ));
    }

    #[test]
    fn unknown_key_is_rejected() {
        let other = SigningKey {
            id: "k2".to_string(),
            secret: "other".to_string(),
        };
        let token = mint(&key(), Uuid::now_v7(), now());
        assert!(matches!(
            verify(&token, &[other], now(), Duration::from_secs(60)),
            Err(SignedTokenError::UnknownKey(_))
        ));
    }

    #[test]
    fn expired_tokens_are_rejected() {
        let token = mint(&key(), Uuid::now_v7(), now());
        let later = now() + chrono::Duration::days(1);
        assert!(matches!(
            verify(&token, &[key()], later, Duration::from_secs(60)),
            Err(SignedTokenError::Expired)
        ));
    }

    #[test]
    fn clock_skew_within_tolerance_is_accepted() {
        let token = mint(&key(), Uuid::now_v7(), now());
        let slightly_later = now() + chrono::Duration::seconds(30);
        assert!(verify(&token, &[key()], slightly_later, Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn rotation_accepts_both_keys() {
        let new_key = SigningKey {
            id: "k2".to_string(),
            secret: "rotated".to_string(),
        };
        let old_token = mint(&key(), Uuid::now_v7(), now());
        let new_token = mint(&new_key, Uuid::now_v7(), now());
        let keys = vec![new_key, key()];
        assert!(verify(&old_token, &keys, now(), Duration::from_secs(60)).is_ok());
        assert!(verify(&new_token, &keys, now(), Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn garbage_is_malformed() {
        for garbage in ["", "v1", "v1.a.b.c", "v2.a.b.c.d", "v1.a.b.c.d"] {
            assert!(matches!(
                verify(garbage, &[key()], now(), Duration::from_secs(60)),
                Err(SignedTokenError::Malformed) | Err(SignedTokenError::UnknownKey(_))
            ));
        }
    }
}
