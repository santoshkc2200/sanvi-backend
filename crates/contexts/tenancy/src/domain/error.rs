use thiserror::Error;

/// Domain errors for the `tenancy` context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TenancyError {
    /// The slug is not a valid DNS-safe subdomain label.
    #[error("invalid tenant slug: {0}")]
    InvalidSlug(String),

    /// The slug is on the reserved-word blocklist.
    #[error("tenant slug is reserved: {slug}")]
    SlugReserved { slug: String },

    /// Another tenant (or a pending reservation) already owns the slug.
    #[error("tenant slug is already taken: {slug}")]
    SlugTaken { slug: String },

    #[error("invalid tenant display name: {0}")]
    InvalidDisplayName(String),

    #[error("invalid region: {0}")]
    InvalidRegion(String),

    #[error("invalid locale: {0}")]
    InvalidLocale(String),

    #[error("tenant {id} not found")]
    TenantNotFound { id: String },

    #[error("illegal state transition: {from} -> {to}")]
    IllegalTransition {
        from: &'static str,
        to: &'static str,
    },

    /// A use case required a tenant but none was supplied.
    #[error("tenant context is missing")]
    MissingTenantContext,

    /// An error this domain does not model (infrastructure failures mapped
    /// at the adapter boundary).
    #[error("storage failure: {0}")]
    Storage(String),

    /// An invalid suspension reason was supplied.
    #[error("invalid suspension reason: {0}")]
    InvalidSuspensionReason(String),

    /// A stored tenant status string could not be parsed.
    #[error("invalid tenant status: {0}")]
    InvalidStatus(String),

    /// A settings update was rejected.
    #[error("invalid settings: {0}")]
    InvalidSettings(String),

    #[error("id generation failed: {0}")]
    IdGeneration(String),
}
