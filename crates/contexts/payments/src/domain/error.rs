//! The payments context's domain errors.
//!
//! The contract fixes a `payments/*` problem-type namespace (see
//! `api/payments.yaml`); `type_slug` is where each class's slug lives so
//! the HTTP adapter can never drift from the documented taxonomy.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PaymentsError {
    /// The provider API is unreachable or answered unusably — the caller
    /// should retry, not change anything. `payments/provider-unavailable`.
    #[error("payment provider unavailable: {0}")]
    ProviderUnavailable(String),

    /// A money-moving action was attempted on a connection that is not
    /// `Active`. `payments/connection-not-active`.
    #[error("payment provider connection is not active: {0}")]
    ConnectionNotActive(String),

    /// The connected account lacks a capability the action needs (go-live
    /// is `card_payments == active`, never a deprecated boolean).
    /// `payments/capability-missing`.
    #[error("capability {capability} is not active: {detail}")]
    CapabilityMissing { capability: String, detail: String },

    /// The connected account cannot process the requested currency
    /// (country/currency mismatch). `payments/currency-unsupported`.
    #[error("currency {currency} is not supported by this connection: {detail}")]
    CurrencyUnsupported { currency: String, detail: String },

    /// The same `Idempotency-Key` was reused with a different payload.
    /// `payments/idempotency-conflict`.
    #[error("idempotency conflict: {0}")]
    IdempotencyConflict(String),

    /// A minor-unit amount or currency code failed validation.
    #[error("invalid money: {0}")]
    InvalidMoney(String),

    /// A connection command failed validation (empty ids, unknown
    /// provider, illegal transition shape).
    #[error("invalid connection: {0}")]
    InvalidConnection(String),

    /// The tenant lacks the entitlement the route requires — surfaced as
    /// 403 (never 404) so the frontend can show an upgrade prompt.
    #[error("this requires the {0} entitlement")]
    NotEntitled(String),

    /// Webhook signature verification failed (wrong secret, replay, or
    /// malformed header). See [`crate::domain::webhook`].
    #[error("webhook rejected: {0}")]
    WebhookRejected(String),

    /// Persistence failure (logged, never leaked to the client).
    #[error("storage failure: {0}")]
    Storage(String),
}

impl PaymentsError {
    /// The stable problem-type slug under the `payments/` namespace. The
    /// platform's RFC 9457 layer prefixes the base URI; slugs are the
    /// contract-stable part.
    pub fn type_slug(&self) -> &'static str {
        match self {
            Self::ProviderUnavailable(_) => "payments/provider-unavailable",
            Self::ConnectionNotActive(_) => "payments/connection-not-active",
            Self::CapabilityMissing { .. } => "payments/capability-missing",
            Self::CurrencyUnsupported { .. } => "payments/currency-unsupported",
            Self::IdempotencyConflict(_) => "payments/idempotency-conflict",
            Self::InvalidMoney(_) | Self::InvalidConnection(_) => "bad-request",
            Self::NotEntitled(_) => "entitlement-required",
            Self::WebhookRejected(_) => "unauthorized",
            Self::Storage(_) => "internal-error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_domain_error_maps_to_a_payments_namespace_slug() {
        // The five contract-fixed classes plus the platform-shared ones.
        assert_eq!(
            PaymentsError::ProviderUnavailable("upstream down".into()).type_slug(),
            "payments/provider-unavailable"
        );
        assert_eq!(
            PaymentsError::ConnectionNotActive("restricted".into()).type_slug(),
            "payments/connection-not-active"
        );
        assert_eq!(
            PaymentsError::CapabilityMissing {
                capability: "card_payments".into(),
                detail: "pending".into()
            }
            .type_slug(),
            "payments/capability-missing"
        );
        assert_eq!(
            PaymentsError::CurrencyUnsupported {
                currency: "JPY".into(),
                detail: "not enabled".into()
            }
            .type_slug(),
            "payments/currency-unsupported"
        );
        assert_eq!(
            PaymentsError::IdempotencyConflict("key reuse".into()).type_slug(),
            "payments/idempotency-conflict"
        );
        // Platform-shared classes stay in the shared namespace.
        assert_eq!(
            PaymentsError::InvalidMoney("x".into()).type_slug(),
            "bad-request"
        );
        assert_eq!(
            PaymentsError::NotEntitled("payments.stripe_connect".into()).type_slug(),
            "entitlement-required"
        );
    }
}
