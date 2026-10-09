//! Sanvi HTTP platform: the shared actix-web machinery every context mounts
//! into.
//!
//! - [`server::bind`] builds the actix server with configured workers,
//!   timeouts, keep-alive and graceful shutdown.
//! - [`middleware::stack`] applies the canonical middleware chain (see its
//!   docs for the full order): metrics → … → CORS → security headers →
//!   plan limits → monthly quotas → abuse quota → compression → etag →
//!   cache headers → error handlers.
//! - [`compression`] encodes responses above a size floor, never twice.
//! - [`etag`] stamps validators on GET/HEAD and answers `304`.
//! - [`error`] defines the RFC 9457 `application/problem+json` model and the
//!   single place where domain errors map to HTTP status codes.
//! - [`openapi`] hosts the generated OpenAPI document and Scalar UI.

pub mod abuse;
pub mod api_limits;
pub mod authn;
pub mod authz;
pub mod cache_headers;
pub mod compression;
pub mod config;
pub mod degraded;
pub mod deprecation;
pub mod error;
pub mod etag;
pub mod external_quotas;
pub mod features;
pub mod impersonation;
pub mod load_shed;
pub mod locale;
pub mod metrics;
pub mod middleware;
pub mod openapi;
pub mod quota_middleware;
pub mod quotas;
pub mod rate_limit;
pub mod server;
pub mod ssr_cache;
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
pub use compression::{CompressionConfig, SUPPORTED_ENCODINGS_LIST, SelectiveCompression};
pub use config::{
    AbuseControlsConfig, CorsConfig, DegradedModeConfig, HstsConfig, LoadSheddingConfig,
    RateLimitConfig, SecurityHeadersConfig, ServerConfig, SurfaceAbuseConfig, TenantPolicyConfig,
    WindowQuota,
};
pub use degraded::{DEGRADED_HEADER, DegradedHeaderLayer};
pub use error::{ApiError, PROBLEM_TYPE_BASE, ProblemDetail, ProblemTranslator};
pub use etag::{ConditionalEtag, etag_for_bytes, if_none_match_matches};
pub use features::{FeatureGate, RequiredAnyFeature, RequiredFeature};
pub use impersonation::{
    IMPERSONATION_BANNER_HEADER, IMPERSONATION_HEADER, ImpersonatedSubject, ImpersonationContext,
    ImpersonationMiddleware, ImpersonationMode, ImpersonationVerifier, impersonation_actor,
};
pub use load_shed::{AdmitOutcome, InFlightGuard, LoadShedState, LoadShedding, ShedReason};
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
