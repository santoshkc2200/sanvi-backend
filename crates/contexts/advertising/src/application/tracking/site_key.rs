//! Signs and verifies the public tracking site key: a per-tenant-domain
//! token that authenticates the *site* posting to `/public/track`, not a
//! user. It is public by nature (shipped in page source), so it is paired
//! with a strict `Origin` check and rate limits rather than treated as a
//! secret in its own right — but the signature itself still must be
//! verified in constant time, or the check leaks its own validity through
//! timing.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use uuid::Uuid;

use crate::domain::AdvertisingError;

type HmacSha256 = Hmac<Sha256>;

fn mac_for(secret: &str, tenant_id: Uuid, origin: &str) -> HmacSha256 {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any size");
    mac.update(tenant_id.as_bytes());
    mac.update(origin.as_bytes());
    mac
}

pub fn generate_site_key(secret: &str, tenant_id: Uuid, origin: &str) -> String {
    let signature = mac_for(secret, tenant_id, origin).finalize().into_bytes();
    let encoded =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, signature);
    format!("{tenant_id}.{encoded}")
}

pub fn verify_site_key(
    secret: &str,
    site_key: &str,
    origin: &str,
) -> Result<Uuid, AdvertisingError> {
    let (tenant_part, signature_part) = site_key
        .split_once('.')
        .ok_or_else(|| AdvertisingError::InvalidInput("invalid site key format".to_string()))?;

    let tenant_id = Uuid::parse_str(tenant_part)
        .map_err(|_| AdvertisingError::InvalidInput("invalid tenant id in site key".to_string()))?;

    let signature_bytes = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        signature_part,
    )
    .map_err(|_| {
        AdvertisingError::InvalidInput("invalid site key signature encoding".to_string())
    })?;

    // `verify_slice` compares in constant time — a plain `==` on the
    // decoded/re-encoded strings would leak how many leading bytes matched
    // through response timing.
    mac_for(secret, tenant_id, origin)
        .verify_slice(&signature_bytes)
        .map_err(|_| AdvertisingError::InvalidInput("site key signature mismatch".to_string()))?;

    Ok(tenant_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_binds_to_origin() {
        let secret = "test-signing-secret";
        let tenant_id = Uuid::now_v7();
        let key = generate_site_key(secret, tenant_id, "https://shop.example.com");

        assert_eq!(
            verify_site_key(secret, &key, "https://shop.example.com").unwrap(),
            tenant_id
        );
        assert!(verify_site_key(secret, &key, "https://evil.example.com").is_err());
        assert!(verify_site_key("wrong-secret", &key, "https://shop.example.com").is_err());
        assert!(verify_site_key(secret, "not-a-site-key", "https://shop.example.com").is_err());
    }
}
