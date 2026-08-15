//! Sanvi HTTP platform: the shared actix-web machinery every context mounts
//! into.
//!
//! - [`server::bind`] builds the actix server with configured workers,
//!   timeouts and graceful shutdown.
//! - [`middleware::stack`] applies the canonical middleware chain:
//!   concurrency limit → request id → tracing span → panic catch → CORS →
//!   security headers → rate limit → tenant resolution → request timeout.
//! - [`error`] defines the RFC 9457 `application/problem+json` model and the
//!   single place where domain errors map to HTTP status codes.
//! - [`openapi`] hosts the generated OpenAPI document and Scalar UI.

pub mod config;
pub mod error;
pub mod metrics;
pub mod middleware;
pub mod openapi;
pub mod rate_limit;
pub mod server;
pub mod tenancy;
pub mod tracing;

pub use config::{CorsConfig, RateLimitConfig, ServerConfig, TenantPolicyConfig};
pub use error::{ApiError, ProblemDetail};
pub use metrics::{HttpInstruments, TenantResolutionInstruments};
pub use middleware::RequestId;
pub use server::{bind, bind_without_run, run, shutdown_signal};
pub use tenancy::{
    CurrentTenant, Resolution, ResolutionSource, TenantContext, TenantMiddleware, TenantResolver,
    TenantRuntimeStatus,
};
