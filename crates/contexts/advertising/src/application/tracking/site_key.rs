//! Signs and verifies the public tracking site key: a per-tenant-domain
//! token that authenticates the *site* posting to `/public/track`, not a
//! user. It is public by nature (shipped in page source), so it is paired
//! with a strict `Origin` check and rate limits rather than treated as a
//! secret in its own right — but the signature itself still must be
//! verified in constant time, or the check leaks its own validity through
//! timing.

use hmac::{Hmac, Mac};
use sanvi_kernel::{TenantId, UserId};
use sha2::Sha256;
use uuid::Uuid;

use crate::domain::AdvertisingError;

type HmacSha256 = Hmac<Sha256>;

/// HMAC accepts a key of any length, including none — so an unconfigured
/// secret still mints and verifies signatures that anyone else can compute
/// for any tenant. Both directions fail closed instead.
/// [`AdvertisingConfig::validate_tracking_enabled`] enforces the same
/// bound at startup; this is the guard that holds if that call is ever
/// bypassed.
pub const MIN_SIGNING_SECRET_BYTES: usize = 32;

fn mac_for(
    secret: &str,
    tenant_id: TenantId,
    origin: &str,
) -> Result<HmacSha256, AdvertisingError> {
    if secret.len() < MIN_SIGNING_SECRET_BYTES {
        return Err(AdvertisingError::InvalidInput(
            "tracking site key signing secret is not configured".to_string(),
        ));
    }
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any size");
    mac.update(tenant_id.to_string().as_bytes());
    mac.update(origin.as_bytes());
    Ok(mac)
}

pub fn generate_site_key(
    secret: &str,
    tenant_id: TenantId,
    origin: &str,
) -> Result<String, AdvertisingError> {
    let signature = mac_for(secret, tenant_id, origin)?.finalize().into_bytes();
    let encoded =
        base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, signature);
    Ok(format!("{tenant_id}.{encoded}"))
}

pub fn verify_site_key(
    secret: &str,
    site_key: &str,
    origin: &str,
) -> Result<TenantId, AdvertisingError> {
    let (tenant_part, signature_part) = site_key
        .split_once('.')
        .ok_or_else(|| AdvertisingError::InvalidInput("invalid site key format".to_string()))?;

    let tenant_id = tenant_part
        .parse::<TenantId>()
        .map_err(|_| AdvertisingError::InvalidInput("invalid tenant id in site key".to_string()))?;

    let mac = mac_for(secret, tenant_id, origin)?;

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
    mac.verify_slice(&signature_bytes)
        .map_err(|_| AdvertisingError::InvalidInput("site key signature mismatch".to_string()))?;

    Ok(tenant_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_binds_to_origin() {
        let secret = "test-signing-secret-of-at-least-32-bytes";
        let tenant_id = TenantId::try_from(1_000_001_i64).unwrap();
        let key = generate_site_key(secret, tenant_id, "https://shop.example.com").unwrap();

        assert_eq!(
            verify_site_key(secret, &key, "https://shop.example.com").unwrap(),
            tenant_id
        );
        assert!(verify_site_key(secret, &key, "https://evil.example.com").is_err());
        assert!(
            verify_site_key(
                "another-signing-secret-of-at-least-32-bytes",
                &key,
                "https://shop.example.com"
            )
            .is_err()
        );
        assert!(verify_site_key(secret, "not-a-site-key", "https://shop.example.com").is_err());
    }

    /// The empty default must never mint or accept a key: `HMAC("")` is
    /// computable by anyone, so it would authenticate every tenant to
    /// every caller.
    #[test]
    fn an_unconfigured_secret_neither_signs_nor_verifies() {
        let tenant_id = TenantId::try_from(1_000_001_i64).unwrap();
        let origin = "https://shop.example.com";
        let real = generate_site_key(
            "test-signing-secret-of-at-least-32-bytes",
            tenant_id,
            origin,
        )
        .unwrap();

        assert!(generate_site_key("", tenant_id, origin).is_err());
        assert!(verify_site_key("", &real, origin).is_err());

        let too_short = "0123456789abcdef0123456789abcde";
        assert_eq!(too_short.len(), MIN_SIGNING_SECRET_BYTES - 1);
        assert!(generate_site_key(too_short, tenant_id, origin).is_err());
        assert!(verify_site_key(too_short, &real, origin).is_err());
    }
}
