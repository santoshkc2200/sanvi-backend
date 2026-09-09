use sanvi_kernel::IdGenerationError;
use thiserror::Error;

/// Whether a failure is worth retrying.
///
/// The Keto sync consumer classifies its errors with this so the messaging
/// layer retries a reachability blip with backoff instead of dead-lettering
/// an authorization change on its first failure. A rejection, by contrast, is
/// the same on every attempt and goes straight to the dead-letter stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryability {
    Transient,
    Permanent,
}

/// Domain errors for the `access` context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AccessError {
    #[error("unknown permission: {0}")]
    UnknownPermission(String),

    #[error("invalid permission key: {0}")]
    InvalidPermission(String),

    #[error("invalid role scope: {0}")]
    InvalidRoleScope(String),

    #[error("invalid role key: {0}")]
    InvalidRoleKey(String),

    #[error("role {key} already exists")]
    RoleAlreadyExists { key: String },

    #[error("role {key} not found")]
    RoleNotFound { key: String },

    #[error("failed to resolve role key for role {0}")]
    RoleKeyResolutionFailed(sanvi_kernel::RoleId),

    #[error("system roles cannot be modified: {key}")]
    SystemRoleImmutable { key: String },

    #[error("permission {permission} is not assignable in scope {scope}")]
    PermissionScopeMismatch { permission: String, scope: String },

    #[error("cannot grant {permission}: the actor does not hold it")]
    Escalation { permission: String },

    #[error("invalid feature key: {0}")]
    InvalidFeatureKey(String),

    #[error("invalid feature definition: {0}")]
    InvalidFeatureDefinition(String),

    #[error("feature {key} already exists")]
    FeatureAlreadyExists { key: String },

    #[error("feature {key} not found")]
    FeatureNotFound { key: String },

    #[error("invalid entitlement: {0}")]
    InvalidEntitlement(String),

    #[error("invalid quota limit: {0}")]
    InvalidLimit(String),

    #[error("entitlement for {feature} not found")]
    EntitlementNotFound { feature: String },

    #[error("quota exceeded for {feature}: limit {limit}, attempted {attempted}")]
    QuotaExceeded {
        feature: String,
        limit: i64,
        attempted: i64,
    },

    #[error("entitlement backend is unavailable: {0}")]
    EntitlementBackendUnavailable(String),

    #[error("authorization backend is unreachable: {0}")]
    BackendUnreachable(String),

    #[error("authorization backend rejected the request: {0}")]
    BackendRejected(String),

    #[error("storage failure")]
    Storage(String),

    #[error("ID generation failure: {0}")]
    IdGeneration(String),
}

impl From<IdGenerationError> for AccessError {
    fn from(error: IdGenerationError) -> Self {
        Self::IdGeneration(error.to_string())
    }
}

impl AccessError {
    /// Classify this error for the consumer's retry policy.
    ///
    /// Transient: the backend could not be reached, storage was unavailable,
    /// or a role key had not replicated yet — all of which a later attempt may
    /// resolve. Everything else describes the *content* of the request and
    /// will fail identically on every retry.
    pub fn retryability(&self) -> Retryability {
        match self {
            Self::BackendUnreachable(_)
            | Self::EntitlementBackendUnavailable(_)
            | Self::Storage(_)
            | Self::RoleKeyResolutionFailed(_) => Retryability::Transient,
            _ => Retryability::Permanent,
        }
    }
}
