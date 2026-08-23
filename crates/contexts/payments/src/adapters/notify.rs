//! Notification and verification adapters for the 09.3 transport.
//!
//! - [`LoggingConnectionNotifier`] renders nothing and sends nothing: it
//!   records lifecycle notifications on the log so every environment has a
//!   working implementation of the port. The worker swaps in a
//!   locale-resolving adapter over the phase-06 localizer; the API and
//!   tests take this one.
//! - [`DenyAllVerifier`] is what the Connect endpoint verifies against
//!   when no provider adapter is configured: every event fails closed.
//!   An unconfigured endpoint rejects rather than trusts.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::domain::error::PaymentsError;
use crate::domain::ports::{ConnectionNotifier, WebhookVerifierPort};

#[derive(Debug, Clone, Copy, Default)]
pub struct LoggingConnectionNotifier;

impl LoggingConnectionNotifier {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ConnectionNotifier for LoggingConnectionNotifier {
    async fn connection_activated(
        &self,
        tenant_id: Uuid,
        connection_id: Uuid,
    ) -> Result<(), PaymentsError> {
        tracing::info!(
            tenant_id = %tenant_id,
            connection_id = %connection_id,
            "payments notification: provider connection activated"
        );
        Ok(())
    }

    async fn connection_restricted(
        &self,
        tenant_id: Uuid,
        connection_id: Uuid,
        detail: &str,
    ) -> Result<(), PaymentsError> {
        tracing::warn!(
            tenant_id = %tenant_id,
            connection_id = %connection_id,
            detail = %detail,
            "payments notification: provider connection restricted"
        );
        Ok(())
    }

    async fn requirement_deadline_approaching(
        &self,
        tenant_id: Uuid,
        connection_id: Uuid,
        deadline: DateTime<Utc>,
    ) -> Result<(), PaymentsError> {
        tracing::info!(
            tenant_id = %tenant_id,
            connection_id = %connection_id,
            deadline = %deadline,
            "payments notification: requirement deadline approaching"
        );
        Ok(())
    }

    async fn dispute_opened(
        &self,
        tenant_id: Uuid,
        dispute_id: Uuid,
        due_by: Option<DateTime<Utc>>,
    ) -> Result<(), PaymentsError> {
        tracing::warn!(
            tenant_id = %tenant_id,
            dispute_id = %dispute_id,
            due_by = ?due_by,
            "payments notification: chargeback dispute opened"
        );
        Ok(())
    }

    async fn payout_failed(
        &self,
        tenant_id: Uuid,
        external_payout_id: &str,
        amount: &crate::domain::Money,
    ) -> Result<(), PaymentsError> {
        tracing::error!(
            tenant_id = %tenant_id,
            payout = %external_payout_id,
            amount_minor = amount.amount_minor,
            currency = amount.currency.as_str(),
            "payments notification: PAYOUT FAILED — the tenant's money is stuck at the provider"
        );
        Ok(())
    }

    async fn tax_compliance_lapsed(
        &self,
        tenant_id: Uuid,
        detail: &str,
    ) -> Result<(), PaymentsError> {
        tracing::warn!(
            tenant_id = %tenant_id,
            detail = %detail,
            "payments notification: tax compliance lapsed while automatic tax is enabled"
        );
        Ok(())
    }
}

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
