//! Connect webhook signature verification.
//!
//! The Connect endpoint (`/internal/webhooks/stripe/connect`, sub-phase
//! 09.3) signs with **its own** secret — distinct from the phase-04
//! platform endpoint's. Same scheme (Stripe's `t=…,v1=…` HMAC-SHA256
//! header, constant-time comparison, timestamp tolerance), separate
//! secret: an event signed for one endpoint must never verify against the
//! other, or a leaked platform secret would forge Connect events (and
//! vice versa). The cross-secret tests below plus the composition root's
//! "the two configured secrets must differ" validation make that a
//! build-time invariant, not an operator memo.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::error::PaymentsError;

type HmacSha256 = Hmac<Sha256>;

/// Why a webhook was rejected. Every variant maps to a 401 at the
/// transport; the detail is logged, never returned raw to the caller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookVerificationError {
    #[error("malformed signature header: {0}")]
    MalformedHeader(String),
    #[error("timestamp {0} outside tolerance ({1}s)")]
    StaleTimestamp(i64, i64),
    #[error("signature mismatch")]
    SignatureMismatch,
}

impl From<WebhookVerificationError> for PaymentsError {
    fn from(err: WebhookVerificationError) -> Self {
        PaymentsError::WebhookRejected(err.to_string())
    }
}

/// Verify a Stripe webhook signature header (`t=...,v1=...`) against the
/// **Connect** signing secret. Pure function — unit-tested against known
/// vectors, including cross-endpoint vectors (a payload signed with the
/// platform endpoint's secret fails here by construction).
pub fn verify_webhook_signature(
    secret: &[u8],
    payload: &[u8],
    signature_header: &str,
    tolerance_secs: i64,
) -> Result<(), WebhookVerificationError> {
    let mut timestamp: Option<i64> = None;
    let mut signature: Option<&str> = None;
    for part in signature_header.split(',') {
        let Some((key, value)) = part.trim().split_once('=') else {
            return Err(WebhookVerificationError::MalformedHeader(
                "malformed signature header".to_string(),
            ));
        };
        match key {
            "t" => timestamp = value.parse().ok(),
            "v1" => signature = Some(value),
            _ => {}
        }
    }
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return Err(WebhookVerificationError::MalformedHeader(
            "signature header missing t or v1".to_string(),
        ));
    };

    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > tolerance_secs {
        return Err(WebhookVerificationError::StaleTimestamp(
            timestamp,
            tolerance_secs,
        ));
    }

    let signed_payload = format!("{timestamp}.{}", String::from_utf8_lossy(payload));
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| WebhookVerificationError::MalformedHeader("bad secret".to_string()))?;
    mac.update(signed_payload.as_bytes());
    let expected = mac.finalize().into_bytes();

    // Constant-time comparison against a decoded candidate.
    let Ok(candidate) = hex::decode(signature) else {
        return Err(WebhookVerificationError::MalformedHeader(
            "signature is not hex".to_string(),
        ));
    };
    if candidate.len() != expected.len() {
        return Err(WebhookVerificationError::SignatureMismatch);
    }
    let mut diff = 0u8;
    for (a, b) in candidate.iter().zip(expected.iter()) {
        diff |= a ^ b;
    }
    if diff != 0 {
        return Err(WebhookVerificationError::SignatureMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two endpoint secrets, deliberately distinct — the deployment
    /// invariant the composition root also validates at boot.
    const CONNECT_SECRET: &str = "whsec_connect_test";
    const PLATFORM_SECRET: &str = "whsec_platform_test";

    fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{timestamp}.{}", String::from_utf8_lossy(payload)).as_bytes());
        let sig = hex::encode(mac.finalize().into_bytes());
        format!("t={timestamp},v1={sig}")
    }

    #[test]
    fn valid_signature_passes() {
        let payload = br#"{"id": "evt_connect_1", "type": "v2.core.account.updated"}"#;
        let header = sign(CONNECT_SECRET, chrono::Utc::now().timestamp(), payload);
        verify_webhook_signature(CONNECT_SECRET.as_bytes(), payload, &header, 300).unwrap();
    }

    #[test]
    fn tampered_payload_or_wrong_secret_fails() {
        let payload = br#"{"id": "evt_connect_1"}"#;
        let header = sign(CONNECT_SECRET, chrono::Utc::now().timestamp(), payload);
        verify_webhook_signature(
            CONNECT_SECRET.as_bytes(),
            br#"{"id": "evt_tampered"}"#,
            &header,
            300,
        )
        .unwrap_err();
        verify_webhook_signature(b"whsec_other", payload, &header, 300).unwrap_err();
    }

    #[test]
    fn stale_timestamps_fail() {
        let payload = br#"{"id": "evt_connect_1"}"#;
        let stale = chrono::Utc::now().timestamp() - 10_000;
        let header = sign(CONNECT_SECRET, stale, payload);
        verify_webhook_signature(CONNECT_SECRET.as_bytes(), payload, &header, 300).unwrap_err();
    }

    #[test]
    fn malformed_headers_fail() {
        let payload = br#"{"id": "evt_connect_1"}"#;
        verify_webhook_signature(CONNECT_SECRET.as_bytes(), payload, "v1=abc", 300).unwrap_err();
        verify_webhook_signature(CONNECT_SECRET.as_bytes(), payload, "t=123,v1=zz", 300)
            .unwrap_err();
    }

    // ------------------------------------------------------------------
    // The 09.0 CI gate: the Connect secret and the phase-04 platform
    // secret are interchangeable with nobody noticing — unless this pair
    // of tests exists. A platform-endpoint event must never verify against
    // the Connect endpoint, and vice versa.
    // ------------------------------------------------------------------

    #[test]
    fn a_platform_signed_event_does_not_verify_against_the_connect_secret() {
        let platform_event = br#"{"id": "evt_platform_1", "type": "invoice.paid"}"#;
        let signed_for_platform = sign(
            PLATFORM_SECRET,
            chrono::Utc::now().timestamp(),
            platform_event,
        );
        assert_eq!(
            verify_webhook_signature(
                CONNECT_SECRET.as_bytes(),
                platform_event,
                &signed_for_platform,
                300
            )
            .unwrap_err(),
            WebhookVerificationError::SignatureMismatch
        );
    }

    #[test]
    fn a_connect_signed_event_does_not_verify_against_the_platform_secret() {
        let connect_event = br#"{"id": "evt_connect_1", "type": "account.updated"}"#;
        let signed_for_connect = sign(
            CONNECT_SECRET,
            chrono::Utc::now().timestamp(),
            connect_event,
        );
        assert_eq!(
            verify_webhook_signature(
                PLATFORM_SECRET.as_bytes(),
                connect_event,
                &signed_for_connect,
                300
            )
            .unwrap_err(),
            WebhookVerificationError::SignatureMismatch
        );
    }
}
