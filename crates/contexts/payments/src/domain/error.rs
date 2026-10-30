//! The problem types the Payments service answers with. The gateway relays
//! them unchanged, so they stay part of the platform's published problem
//! catalog (`api/shared/common.yaml` is generated from this `type_slug`).
//! Keep this list in step with the service's `PaymentsError::type_slug`.

/// One wire problem class; the variants mirror the service's error kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaymentsProblem {
    ProviderUnavailable,
    ConnectionNotActive,
    CapabilityMissing,
    CurrencyUnsupported,
    IdempotencyConflict,
    ProviderAuth,
    ProviderRateLimited,
    InvalidCountry,
    CountryUnsupported,
    ConnectionExists,
    DisconnectBlocked,
    DomainNotAllowed,
    RateLimited,
    RefundExceedsRemaining,
    TaxPreflightFailed,
    BadRequest,
    NotEntitled,
    WebhookRejected,
    Internal,
}

impl PaymentsProblem {
    /// The stable problem-type slug under the `payments/` namespace. The
    /// platform's RFC 9457 layer prefixes the base URI; slugs are the
    /// contract-stable part.
    pub fn type_slug(&self) -> &'static str {
        match self {
            Self::ProviderUnavailable => "payments/provider-unavailable",
            Self::ConnectionNotActive => "payments/connection-not-active",
            Self::CapabilityMissing => "payments/capability-missing",
            Self::CurrencyUnsupported => "payments/currency-unsupported",
            Self::IdempotencyConflict => "payments/idempotency-conflict",
            Self::ProviderAuth => "payments/provider-auth",
            Self::ProviderRateLimited => "payments/provider-rate-limited",
            Self::InvalidCountry => "payments/invalid-country",
            Self::CountryUnsupported => "payments/country-unsupported",
            Self::ConnectionExists => "payments/connection-exists",
            Self::DisconnectBlocked => "payments/disconnect-blocked",
            Self::DomainNotAllowed => "payments/domain-not-allowed",
            Self::RateLimited => "payments/rate-limited",
            Self::RefundExceedsRemaining => "payments/over-refund",
            Self::TaxPreflightFailed => "payments/tax-preflight-failed",
            Self::BadRequest => "bad-request",
            Self::NotEntitled => "entitlement-required",
            Self::WebhookRejected => "unauthorized",
            Self::Internal => "internal-error",
        }
    }
}
