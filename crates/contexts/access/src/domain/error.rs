use thiserror::Error;
use sanvi_kernel::IdGenerationError;

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
