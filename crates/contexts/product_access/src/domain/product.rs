//! The product services the platform delegates to, and what each one needs
//! from the platform to authorize a request.

/// A separately sold product service behind the platform proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Product {
    /// Short name used in routes and logs.
    pub key: &'static str,
    /// Audience the service expects in identity tokens.
    pub audience: &'static str,
    /// Boolean entitlement a tenant needs to use the product at all.
    pub feature: &'static str,
    /// Only permissions with this prefix are delegated to the service.
    pub permission_prefix: &'static str,
    /// Quota entitlements whose limits the service enforces itself.
    pub limits: &'static [&'static str],
}

/// Sanvi Ecommerce (`ecommerce-service`).
pub const ECOMMERCE: Product = Product {
    key: "ecommerce",
    audience: "ecommerce-service",
    feature: "products.ecommerce.enabled",
    permission_prefix: "ecommerce.",
    limits: &["ecommerce.stores.max"],
};

/// Issuer every delegation token names.
pub const ISSUER: &str = "sanvi-backend";
