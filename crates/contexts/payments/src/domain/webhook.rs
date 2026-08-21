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

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

use super::error::PaymentsError;

type HmacSha256 = Hmac<Sha256>;

/// One stored Connect webhook event (`payments.webhook_events`). The row
/// is the unit of dedupe (PK on the provider's event id), retry
/// (`attempts`) and dead-lettering (`attempts` exhausted) — the transport's
/// whole state lives here, never in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookEventRecord {
    pub stripe_event_id: String,
    /// The connected account the event is about (`acct_…`), when the
    /// payload names one. Events without a routable account are stored,
    /// alerted and acknowledged — never guessed.
    pub account_id: Option<String>,
    pub event_type: String,
    pub api_version: Option<String>,
    pub payload: Value,
    /// The provider's own event time — the out-of-order anchor, never our
    /// processing clock.
    pub created: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub attempts: i32,
    /// The last processing error, if any (backs the dead-letter triage
    /// view; cleared on success).
    pub last_error: Option<String>,
}

/// Extract the connected-account id from a webhook payload, tolerating the
/// envelope shapes Stripe actually sends:
///
/// - v1 envelopes put it top-level (`"account": "acct_…"`);
/// - capability objects carry it as `data.object.account`;
/// - Accounts-v2 core events reference it as `related_object.id`;
/// - account objects are themselves `data.object` with an `acct_…` id.
///
/// Anything else (or an id that is not an account id) yields `None` — the
/// dispatcher records and alerts rather than guessing a tenant.
pub fn event_account_id(payload: &Value) -> Option<String> {
    payload
        .get("account")
        .and_then(Value::as_str)
        .or_else(|| {
            payload
                .pointer("/data/object/account")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            payload
                .pointer("/related_object/id")
                .and_then(Value::as_str)
        })
        .or_else(|| payload.pointer("/data/object/id").and_then(Value::as_str))
        .filter(|id| id.starts_with("acct_"))
        .map(str::to_string)
}

/// The provider's event time from the payload, falling back to the
/// receiver's clock when the envelope omits it (it never should; the
/// fallback keeps a malformed-but-signed event storable).
pub fn event_created(payload: &Value, fallback: DateTime<Utc>) -> DateTime<Utc> {
    payload
        .get("created")
        .and_then(Value::as_i64)
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
        .unwrap_or(fallback)
}

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

    // ------------------------------------------------------------------
    // Account routing: every envelope shape Stripe actually sends must
    // resolve to the connected account — or to None, never a guess.
    // ------------------------------------------------------------------

    #[test]
    fn account_ids_extract_from_every_envelope_shape() {
        // v1 envelope: top-level `account`.
        let v1 = serde_json::json!({
            "id": "evt_1", "type": "account.updated",
            "account": "acct_top",
            "data": {"object": {"id": "acct_top"}}
        });
        assert_eq!(event_account_id(&v1).as_deref(), Some("acct_top"));

        // capability.updated: the object id is the capability; the account
        // rides on the object.
        let capability = serde_json::json!({
            "id": "evt_2", "type": "capability.updated",
            "data": {"object": {"id": "card_payments", "account": "acct_cap"}}
        });
        assert_eq!(event_account_id(&capability).as_deref(), Some("acct_cap"));

        // Accounts-v2 core event: related_object points at the account.
        let v2 = serde_json::json!({
            "id": "evt_3", "type": "v2.core.account.updated",
            "related_object": {"id": "acct_v2", "type": "account"}
        });
        assert_eq!(event_account_id(&v2).as_deref(), Some("acct_v2"));

        // No routable account anywhere: None, not a guess.
        let orphan = serde_json::json!({"id": "evt_4", "type": "payout.paid"});
        assert_eq!(event_account_id(&orphan), None);
    }

    #[test]
    fn event_time_comes_from_the_payload_with_a_fallback() {
        let payload = serde_json::json!({"created": 1_800_000_000});
        assert_eq!(
            event_created(&payload, chrono::DateTime::<Utc>::UNIX_EPOCH).timestamp(),
            1_800_000_000
        );
        let fallback = Utc::now();
        assert_eq!(event_created(&serde_json::json!({}), fallback), fallback);
    }
}
