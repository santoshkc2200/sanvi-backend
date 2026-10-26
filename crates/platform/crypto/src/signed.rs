//! HMAC-SHA256 signed JSON claims in the `v1.<base64url payload>.<base64url
//! signature>` layout, for values that cross a browser redirect (OAuth state).
//!
//! Signing only proves the claims were issued by this server. Callers still
//! check expiry and binding, and burn a server-side nonce for single use.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Serialize, de::DeserializeOwned};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignedError {
    #[error("signed value is missing or malformed")]
    Malformed,
    #[error("signed value has an invalid signature")]
    Forged,
}

fn mac(key: &[u8]) -> HmacSha256 {
    HmacSha256::new_from_slice(key).expect("HMAC accepts all key sizes")
}

pub fn sign<T: Serialize>(key: &[u8], claims: &T) -> String {
    let payload = serde_json::to_vec(claims).expect("signed claims serialize");
    let encoded = URL_SAFE_NO_PAD.encode(payload);
    let mut mac = mac(key);
    mac.update(encoded.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("v1.{encoded}.{signature}")
}

/// The signature is checked before the payload is decoded.
pub fn verify<T: DeserializeOwned>(key: &[u8], value: &str) -> Result<T, SignedError> {
    let mut parts = value.split('.');
    let (Some("v1"), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(SignedError::Malformed);
    };
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| SignedError::Malformed)?;
    let mut mac = mac(key);
    mac.update(payload.as_bytes());
    mac.verify_slice(&signature)
        .map_err(|_| SignedError::Forged)?;
    let raw = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| SignedError::Malformed)?;
    serde_json::from_slice(&raw).map_err(|_| SignedError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_accepts_its_own_signature_and_rejects_others() {
        let signed = sign(b"key-a", &("tenant", 7));
        assert_eq!(verify::<(String, i32)>(b"key-a", &signed).unwrap().1, 7);
        assert_eq!(
            verify::<(String, i32)>(b"key-b", &signed),
            Err(SignedError::Forged)
        );
        assert_eq!(
            verify::<(String, i32)>(b"key-a", &format!("{signed}x")),
            Err(SignedError::Forged)
        );
        assert_eq!(
            verify::<(String, i32)>(b"key-a", "v2.a.b"),
            Err(SignedError::Malformed)
        );
        assert_eq!(
            verify::<(String, i32)>(b"key-a", &format!("{signed}.extra")),
            Err(SignedError::Malformed)
        );
    }
}
