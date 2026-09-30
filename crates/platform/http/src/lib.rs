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

pub mod abuse;
pub mod authn;
pub mod authz;
pub mod config;
pub mod error;
pub mod features;
pub mod impersonation;
pub mod locale;
pub mod metrics;
pub mod middleware;
pub mod openapi;
pub mod rate_limit;
pub mod server;
pub mod tenancy;
pub mod trace_context;
pub mod tracing;

pub use abuse::{AbuseCounter, AbuseSurface, QuotaVerdict};
pub use authn::{
    AuthnMiddleware, CurrentSubject, FreshAal2Policy, FreshAal2Required, OptionalSubject,
    SessionChannel, SessionToken, SessionVerifier, Subject,
};
pub use authz::{
    Decision, PermissionGate, PermissionMap, RequiredPermission, ResourceRef, ResourceSpec,
};
pub use config::{
    AbuseControlsConfig, CorsConfig, HstsConfig, RateLimitConfig, SecurityHeadersConfig,
    ServerConfig, SurfaceAbuseConfig, TenantPolicyConfig, WindowQuota,
};
pub use error::{ApiError, PROBLEM_TYPE_BASE, ProblemDetail, ProblemTranslator};
pub use features::{FeatureGate, RequiredAnyFeature, RequiredFeature};
pub use impersonation::{
    IMPERSONATION_BANNER_HEADER, IMPERSONATION_HEADER, ImpersonatedSubject, ImpersonationContext,
    ImpersonationMiddleware, ImpersonationMode, ImpersonationVerifier, impersonation_actor,
};
pub use locale::{
    CurrentLocale, LocaleContext, LocaleMiddleware, LocaleResolutionInput, LocaleResolver,
    LocaleSource,
};
pub use metrics::{AbuseInstruments, HttpInstruments, TenantResolutionInstruments};
pub use middleware::RequestId;
pub use server::{bind, bind_without_run, run, shutdown_signal};
pub use tenancy::{
    CurrentTenant, Resolution, ResolutionSource, TenantContext, TenantMiddleware, TenantResolver,
    TenantRuntimeStatus,
};
