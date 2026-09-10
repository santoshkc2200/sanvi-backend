use async_trait::async_trait;

use crate::domain::error::PaymentsError;
use crate::domain::ports::WebhookVerifierPort;

/// The fail-closed verifier for an endpoint with no configured provider:
/// signature verification cannot succeed because there is no secret to
/// verify against. Never a `Ok(())`.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAllVerifier;

impl DenyAllVerifier {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl WebhookVerifierPort for DenyAllVerifier {
    async fn verify(&self, _payload: &[u8], _signature_header: &str) -> Result<(), PaymentsError> {
        Err(PaymentsError::WebhookRejected(
            "no connect webhook verifier is configured".to_string(),
        ))
    }
}
