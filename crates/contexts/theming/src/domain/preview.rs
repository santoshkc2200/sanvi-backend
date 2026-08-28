//! Signed preview tokens: the draft state may be previewed by authorised
//! users only, through a short-lived HMAC-signed token (no cookie, no
//! cache, `noindex` headers on the delivery path).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::domain::error::ThemingError;
use crate::domain::tenant_theme::ThemeState;

type HmacSha256 = Hmac<Sha256>;

/// The signed payload: which tenant, which state, until when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewTokenPayload {
    pub tenant_id: TenantId,
    pub state: String,
    pub expires_at: DateTime<Utc>,
}

/// Sign a preview token.
pub fn sign_preview_token(
    secret: &[u8],
    tenant_id: TenantId,
    state: ThemeState,
    ttl_secs: u64,
    now: DateTime<Utc>,
) -> String {
    let payload = PreviewTokenPayload {
        tenant_id,
        state: state.as_str().to_string(),
        expires_at: now + chrono::Duration::seconds(ttl_secs as i64),
    };
    let body = serde_json::to_string(&payload).expect("preview payload serializes");
    let encoded = URL_SAFE_NO_PAD.encode(body.as_bytes());
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(encoded.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{encoded}.{signature}")
}

/// Verify a preview token, returning the payload when the signature matches
/// and the token has not expired.
pub fn verify_preview_token(
    secret: &[u8],
    token: &str,
    now: DateTime<Utc>,
) -> Result<PreviewTokenPayload, ThemingError> {
    let Some((encoded, signature)) = token.split_once('.') else {
        return Err(ThemingError::InvalidPreviewToken);
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(encoded.as_bytes());
    let expected = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    // Constant-time comparison.
    let (expected, signature) = (expected.as_bytes(), signature.as_bytes());
    if expected.len() != signature.len()
        || !expected
            .iter()
            .zip(signature.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            .eq(&0)
    {
        return Err(ThemingError::InvalidPreviewToken);
    }
    let body = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ThemingError::InvalidPreviewToken)?;
    let payload: PreviewTokenPayload =
        serde_json::from_slice(&body).map_err(|_| ThemingError::InvalidPreviewToken)?;
    if payload.expires_at <= now {
        return Err(ThemingError::PreviewTokenExpired);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn round_trip() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let tenant = TenantId::try_from(1_000_001_i64).unwrap();
        let token = sign_preview_token(b"secret", tenant, ThemeState::Draft, 60, now);
        let payload = verify_preview_token(b"secret", &token, now).unwrap();
        assert_eq!(payload.tenant_id, tenant);
        assert_eq!(payload.state, "draft");
    }

    #[test]
    fn rejects_tampering_wrong_key_and_expiry() {
        let now = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let tenant = TenantId::try_from(1_000_001_i64).unwrap();
        let token = sign_preview_token(b"secret", tenant, ThemeState::Draft, 60, now);

        assert!(matches!(
            verify_preview_token(b"other", &token, now),
            Err(ThemingError::InvalidPreviewToken)
        ));
        let mut tampered = token.clone();
        tampered.push('x');
        assert!(verify_preview_token(b"secret", &tampered, now).is_err());
        assert!(matches!(
            verify_preview_token(b"secret", &token, now + chrono::Duration::seconds(61)),
            Err(ThemingError::PreviewTokenExpired)
        ));
        assert!(matches!(
            verify_preview_token(b"secret", "garbage", now),
            Err(ThemingError::InvalidPreviewToken)
        ));
    }
}
