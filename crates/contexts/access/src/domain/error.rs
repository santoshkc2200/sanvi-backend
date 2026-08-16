use thiserror::Error;

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

    #[error("authorization backend is unreachable: {0}")]
    BackendUnreachable(String),

    #[error("authorization backend rejected the request: {0}")]
    BackendRejected(String),

    #[error("storage failure")]
    Storage(String),
}
