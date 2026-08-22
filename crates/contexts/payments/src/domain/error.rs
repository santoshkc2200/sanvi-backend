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

    /// The provider rejected our credentials — a configuration failure,
    /// alerted and never retried. `payments/provider-auth`.
    #[error("payment provider authentication failed")]
    ProviderAuth,

    /// The provider is rate-limiting us and the in-client retries (with
    /// backoff) were exhausted. `payments/provider-rate-limited`.
    #[error("payment provider rate limit exhausted: {0}")]
    ProviderRateLimited(String),

    /// A country value the provider cannot even parse (not a valid
    /// ISO-3166-1 code at the provider). `payments/invalid-country`.
    #[error("invalid country: {0}")]
    InvalidCountry(String),

    /// The provider cannot onboard accounts in this country — the tenant
    /// must pick a supported one. `payments/country-unsupported`.
    #[error("country {0} is not supported by this provider")]
    CountryUnsupported(String),

    /// The tenant already has a live connection to this provider; a new
    /// one is refused (reconnecting is a new row after disconnect).
    /// `payments/connection-exists`.
    #[error("connection already exists: {0}")]
    ConnectionExists(String),

    /// A success/cancel URL is not on one of the tenant's *verified*
    /// domains (phase 08). An open redirect here is a phishing
    /// primitive: allow-list, never sanitize. `payments/domain-not-allowed`.
    #[error("domain not allowed: {0}")]
    DomainNotAllowed(String),

    /// Checkout creation exceeded a per-tenant / per-IP / per-customer
    /// rate limit. `payments/rate-limited`.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// A refund would exceed the remaining refundable amount (the
    /// payment's total minus already-refunded succeeded rows). Contains the
    /// remaining minor-unit amount and its currency so the response can name
    /// it, not a generic 422. `payments/over-refund`.
    #[error(
        "refund of {requested_minor} {requested_currency} exceeds remaining refundable {remaining_minor} {remaining_currency}"
    )]
    RefundExceedsRemaining {
        requested_minor: i64,
        requested_currency: String,
        remaining_minor: i64,
        remaining_currency: String,
    },

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
            Self::ProviderAuth => "payments/provider-auth",
            Self::ProviderRateLimited(_) => "payments/provider-rate-limited",
            Self::InvalidCountry(_) => "payments/invalid-country",
            Self::CountryUnsupported(_) => "payments/country-unsupported",
            Self::ConnectionExists(_) => "payments/connection-exists",
            Self::DomainNotAllowed(_) => "payments/domain-not-allowed",
            Self::RateLimited(_) => "payments/rate-limited",
            Self::RefundExceedsRemaining { .. } => "payments/over-refund",
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

    #[test]
    fn every_stripe_error_class_has_its_own_payments_problem_type() {
        // 09.2's taxonomy: each Stripe failure class the adapter can hit
        // maps to its own addressable problem type — never a generic 500.
        assert_eq!(
            PaymentsError::ProviderAuth.type_slug(),
            "payments/provider-auth"
        );
        assert_eq!(
            PaymentsError::ProviderRateLimited("retries exhausted".into()).type_slug(),
            "payments/provider-rate-limited"
        );
        assert_eq!(
            PaymentsError::InvalidCountry("X1".into()).type_slug(),
            "payments/invalid-country"
        );
        assert_eq!(
            PaymentsError::CountryUnsupported("XX".into()).type_slug(),
            "payments/country-unsupported"
        );
        assert_eq!(
            PaymentsError::ConnectionExists("active".into()).type_slug(),
            "payments/connection-exists"
        );
        // 09.4's additions: the verified-domain allow-list and the
        // checkout throttle each get their own addressable problem type.
        assert_eq!(
            PaymentsError::DomainNotAllowed("evil.test".into()).type_slug(),
            "payments/domain-not-allowed"
        );
        assert_eq!(
            PaymentsError::RateLimited("tenant bucket".into()).type_slug(),
            "payments/rate-limited"
        );
        // 09.5: over-refund carries the remaining amount, not a generic 422.
        assert_eq!(
            PaymentsError::RefundExceedsRemaining {
                requested_minor: 3000,
                requested_currency: "JPY".into(),
                remaining_minor: 2000,
                remaining_currency: "JPY".into()
            }
            .type_slug(),
            "payments/over-refund"
        );
    }
}
