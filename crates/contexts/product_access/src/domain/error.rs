use thiserror::Error;

/// Why a product request was not forwarded, or failed upstream.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProductAccessError {
    /// The tenant's plan does not include the product.
    #[error("feature not enabled: {feature}")]
    NotEntitled { feature: &'static str },
    /// The user holds none of the product's permissions in this tenant
    /// (including a user who is not an active member).
    #[error("no {product} permissions in this tenant")]
    NoPermissions { product: &'static str },
    /// The forwarded path or headers are not acceptable.
    #[error("{0}")]
    BadRequest(String),
    /// The product's upstream is not configured, unreachable or timed out.
    #[error("product service unavailable: {0}")]
    Unavailable(String),
    /// The upstream answered in a way that means misconfiguration (for
    /// example it rejected the platform's own credentials).
    #[error("product service misbehaved: {0}")]
    BadGateway(String),
    /// Platform-side lookups (entitlements, roles) failed.
    #[error("storage failure: {0}")]
    Storage(String),
}
