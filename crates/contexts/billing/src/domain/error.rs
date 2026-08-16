//! Domain errors for the billing context. Contexts map these into their own
//! error vocabulary at the boundary; this type never carries infrastructure
//! errors.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum BillingError {
    #[error("invalid plan key: {0}")]
    InvalidPlanKey(String),

    #[error("invalid plan name: {0}")]
    InvalidPlanName(String),

    #[error("invalid plan tier: {0}")]
    InvalidPlanTier(String),

    #[error("invalid plan visibility: {0}")]
    InvalidPlanVisibility(String),

    #[error("invalid price: {0}")]
    InvalidPrice(String),

    #[error("invalid currency: {0}")]
    InvalidCurrency(String),

    #[error("invalid interval: {0}")]
    InvalidInterval(String),

    #[error("invalid entitlement grant: {0}")]
    InvalidEntitlement(String),

    #[error("plan not found: {0}")]
    PlanNotFound(String),

    #[error("price not found: {0}")]
    PriceNotFound(String),

    #[error("plan price is not active: {0}")]
    PriceNotActive(String),

    #[error("plan is not publicly visible: {0}")]
    PlanNotPublic(String),

    #[error("subscription not found: {0}")]
    SubscriptionNotFound(String),

    #[error("tenant already has a subscription")]
    SubscriptionExists,

    #[error("tenant has no active subscription")]
    SubscriptionNotActive,

    #[error("plan key already in use: {0}")]
    PlanKeyTaken(String),

    #[error("invalid webhook payload: {0}")]
    InvalidWebhookPayload(String),

    #[error("webhook signature verification failed: {0}")]
    InvalidWebhookSignature(String),

    #[error("unknown webhook event type: {0}")]
    UnknownWebhookEvent(String),

    #[error("webhook event processing failed: {0}")]
    WebhookProcessing(String),

    #[error("stripe rejected the request as unauthorized (check the restricted key scopes)")]
    StripeUnauthorized,

    #[error("stripe rate limited the request")]
    StripeRateLimited,

    #[error("stripe rejected the request: {detail}")]
    StripeBadRequest { detail: String },

    #[error("stripe service failure: {detail}")]
    StripeService { detail: String },

    #[error("could not reach stripe: {detail}")]
    StripeConnection { detail: String },

    #[error(
        "automatic_tax is enabled but no active Stripe Tax registration exists — tax would silently not be collected"
    )]
    TaxRegistrationMissing,

    #[error("tenant not found: {0}")]
    TenantNotFound(String),

    #[error("state conflict: {0}")]
    Conflict(String),

    #[error("storage failure: {0}")]
    Storage(String),
}
