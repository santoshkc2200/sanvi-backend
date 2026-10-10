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

    /// The offboarding request's state machine refuses the move (e.g.
    /// the purge before a verified export, or re-opening a terminal
    /// request).
    #[error("offboarding request {request_id}: illegal transition {from} -> {to}")]
    OffboardingTransition {
        request_id: i64,
        from: String,
        to: String,
    },

    /// The purge step was reached without a verified export (FR-1131's
    /// hard precondition).
    #[error(
        "offboarding request {request_id}: the purge is unreachable before the export verifies"
    )]
    OffboardingExportMissing { request_id: i64 },

    /// A request conflicts with the tenant's current state (e.g.
    /// offboarding a tenant that is not archived).
    #[error("{0}")]
    Conflict(String),

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
