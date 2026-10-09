//! Tenant resolution: the "which tenant is this?" middleware.
//!
//! [`TenantResolver`] is the port: the tenancy context supplies an
//! implementation (subdomain → slug, signed internal header, custom-domain
//! hook). The middleware enforces the policy around it:
//!
//! - Public paths (health, platform admin, public endpoints) never require a
//!   context.
//! - Tenant-scoped paths with an unresolvable host → `404` (never `403`, to
//!   avoid tenant enumeration).
//! - Suspended tenants → `423 Locked` with a machine-readable `reason`,
//!   except on the suspended allow-list.
//! - On success the [`TenantContext`] is inserted into request extensions and
//!   recorded on the tracing span.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use actix_web::{
    HttpMessage,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
    http::StatusCode,
};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::config::TenantPolicyConfig;
use crate::error::{ApiError, ProblemDetail};
use crate::middleware::RequestId;
use crate::rate_limit::RateLimiter;

/// What a tenant looks like at request time — the shape every context reads
/// from request extensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TenantContext {
    #[schema(value_type = crate::openapi::SnowflakeId)]
    pub tenant_id: TenantId,
    pub slug: String,
    pub display_name: String,
    pub status: TenantRuntimeStatus,
    pub region: String,
    pub default_locale: String,
    pub resolution_source: ResolutionSource,
}

/// Tenant lifecycle state as observed at runtime. Only `Active` tenants serve
/// traffic; every other state returns 423 except on the allow-list, with the
/// machine-readable `reason` naming the state (`restoring` included, so the
/// shell gates during a single-tenant restore).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TenantRuntimeStatus {
    Provisioning,
    Active,
    Suspended,
    Archived,
    Restoring,
}

/// How the tenant was resolved. Most-specific sources win; the source is
/// recorded on the context for observability and debugging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionSource {
    /// Signed `x-sanvi-tenant` header (service-to-service, worker).
    InternalHeader,
    /// Session/JWT tenant claim (phase 02).
    SessionClaim,
    /// Custom domain via the phase-08 lookup hook.
    CustomDomain,
    /// `<slug>.<base-domain>` subdomain of the Host header.
    Subdomain,
}

impl std::fmt::Display for ResolutionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InternalHeader => write!(f, "internal_header"),
            Self::SessionClaim => write!(f, "session_claim"),
            Self::CustomDomain => write!(f, "custom_domain"),
            Self::Subdomain => write!(f, "subdomain"),
        }
    }
}

/// Resolution outcome: found, not found, or an actionable rejection.
pub enum Resolution {
    Found(TenantContext),
    NotFound,
    /// The internal header was present but invalid (tampered, expired, or
    /// signed by an unknown key). Rejected loudly: silently ignoring an
    /// invalid header would let a caller strip it and fall through.
    Rejected {
        detail: String,
    },
}

/// Port for tenant resolution, implemented by the tenancy context.
pub trait TenantResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        internal_header: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<Resolution, ApiError>> + Send + 'a>>;
}

/// Everything the tenant middlewares need.
pub struct TenantMiddleware {
    pub resolver: Arc<dyn TenantResolver>,
    pub policy: TenantPolicyConfig,
}

#[derive(Clone)]
pub(crate) struct TenantResolutionLayer {
    parts: Option<Arc<TenantMiddleware>>,
}

impl TenantResolutionLayer {
    pub fn new(parts: Option<Arc<TenantMiddleware>>) -> Self {
        Self { parts }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for TenantResolutionLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = TenantResolutionService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(TenantResolutionService {
            service: std::rc::Rc::new(service),
            parts: self.parts.clone(),
        }))
    }
}

pub(crate) struct TenantResolutionService<S> {
    service: std::rc::Rc<S>,
    parts: Option<Arc<TenantMiddleware>>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for TenantResolutionService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let service = std::rc::Rc::clone(&self.service);
        let Some(parts) = self.parts.clone() else {
            // Tenancy context not wired: every route behaves public.
            return Box::pin(async move { service.call(req).await });
        };
        Box::pin(async move {
            let path = req.path().to_string();
            if parts.policy.is_public_path(&path) {
                return service.call(req).await;
            }

            let request_id = req.extensions().get::<RequestId>().copied();
            let started = std::time::Instant::now();
            let host = req
                .headers()
                .get(actix_web::http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let internal_header = req
                .headers()
                .get("x-sanvi-tenant")
                .and_then(|value| value.to_str().ok());

            let outcome = parts.resolver.resolve(&host, internal_header).await;
            crate::metrics::TenantResolutionInstruments::process().record(
                started.elapsed(),
                outcome
                    .as_ref()
                    .ok()
                    .and_then(|resolution| match resolution {
                        Resolution::Found(context) => Some(context.resolution_source.to_string()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "unresolved".to_string()),
            );

            let context = match outcome {
                Ok(Resolution::Found(context)) => context,
                Ok(Resolution::NotFound) => {
                    // Never 403: tenant enumeration must not be possible.
                    tracing::debug!(
                        host = %host,
                        path = %path,
                        request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                        "tenant resolution failed: unknown tenant",
                    );
                    let response = not_found_response(&path, request_id);
                    return Ok(req.into_response(response));
                }
                Ok(Resolution::Rejected { detail }) => {
                    tracing::warn!(
                        host = %host,
                        path = %path,
                        detail = %detail,
                        "internal tenant header rejected",
                    );
                    let problem = ApiError::Forbidden { detail }
                        .with_context(Some(path), request_id.map(|id| id.to_string()));
                    let response: actix_web::HttpResponse =
                        actix_web::HttpResponse::Forbidden().json(problem);
                    return Ok(req.into_response(response.map_into_boxed_body()));
                }
                Err(err) => {
                    // Cache/DB backend trouble must never "fail open" into a
                    // request with no tenant: it degrades to a 503.
                    tracing::error!(
                        error = %err,
                        host = %host,
                        "tenant resolution backend failure",
                    );
                    let problem = ApiError::ServiceUnavailable {
                        detail: "tenant resolution is temporarily unavailable".to_string(),
                    }
                    .with_context(Some(path), request_id.map(|id| id.to_string()));
                    let response: actix_web::HttpResponse =
                        actix_web::HttpResponse::ServiceUnavailable().json(problem);
                    return Ok(req.into_response(response.map_into_boxed_body()));
                }
            };

            // Only Active tenants serve normal traffic: Provisioning hasn't
            // finished setup, Suspended and Archived are both explicitly
            // read/admin-only per the phase-01 domain invariants. All three
            // degrade to the same 423, differentiated by the machine-readable
            // `reason`, except on the allow-list (billing, etc).
            if context.status != TenantRuntimeStatus::Active
                && !parts.policy.is_suspended_allowlisted(&path)
            {
                let problem = locked_problem(context.status, &path, request_id);
                let response: actix_web::HttpResponse =
                    actix_web::HttpResponse::Locked().json(problem);
                return Ok(req.into_response(response.map_into_boxed_body()));
            }

            req.extensions_mut().insert(context.clone());
            let span = tracing::Span::current();
            span.record("sanvi.tenant_id", context.tenant_id.to_string());
            span.record("sanvi.tenant_slug", context.slug.clone());
            span.record(
                "sanvi.tenant_resolution_source",
                context.resolution_source.to_string(),
            );

            service.call(req).await
        })
    }
}

fn not_found_response(
    path: &str,
    request_id: Option<RequestId>,
) -> actix_web::HttpResponse<BoxBody> {
    let problem = ApiError::NotFound {
        resource: "tenant".to_string(),
    }
    .with_context(Some(path.to_string()), request_id.map(|id| id.to_string()));
    actix_web::HttpResponse::NotFound()
        .json(problem)
        .map_into_boxed_body()
}

/// Build the 423 problem body for a tenant that isn't `Active`. `reason` is
/// the machine-readable differentiator (`"suspended"`, `"provisioning"`,
/// `"archived"`, `"restoring"`); title and `type` follow it so a restoring
/// tenant isn't reported as "suspended".
fn locked_problem(
    status: TenantRuntimeStatus,
    path: &str,
    request_id: Option<RequestId>,
) -> ProblemDetail {
    let (title, reason) = match status {
        TenantRuntimeStatus::Suspended => ("Tenant Suspended", "suspended"),
        TenantRuntimeStatus::Provisioning => ("Tenant Provisioning", "provisioning"),
        TenantRuntimeStatus::Archived => ("Tenant Archived", "archived"),
        TenantRuntimeStatus::Restoring => ("Tenant Restoring", "restoring"),
        TenantRuntimeStatus::Active => unreachable!("Active tenants are never locked"),
    };
    let mut problem = ApiError::TenantSuspended
        .with_context(Some(path.to_string()), request_id.map(|id| id.to_string()));
    problem.r#type = format!("{}/tenant-{reason}", crate::error::PROBLEM_TYPE_BASE);
    problem.title = title.to_string();
    problem.reason = Some(reason.to_string());
    problem
}

/// Extractor: the resolved tenant for this request.
///
/// Handlers on tenant-scoped routes use this; the middleware guarantees the
/// context is present (or the request never reaches the handler).
#[derive(Clone)]
pub struct CurrentTenant(pub TenantContext);

impl actix_web::FromRequest for CurrentTenant {
    type Error = ApiError;
    type Future = std::future::Ready<Result<Self, Self::Error>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        match req.extensions().get::<TenantContext>() {
            Some(context) => std::future::ready(Ok(Self(context.clone()))),
            None => std::future::ready(Err(ApiError::NotFound {
                resource: "tenant".to_string(),
            })),
        }
    }
}

/// Unit-testable policy decisions extracted from the middleware plumbing.
pub fn status_for_resolution(
    policy: &TenantPolicyConfig,
    path: &str,
    status: TenantRuntimeStatus,
) -> Option<StatusCode> {
    if policy.is_public_path(path) {
        return None;
    }
    if status != TenantRuntimeStatus::Active && !policy.is_suspended_allowlisted(path) {
        return Some(StatusCode::LOCKED);
    }
    None
}

pub const TENANT_HEADER: &str = "x-sanvi-tenant";
pub const TENANT_HEADER_TTL: Duration = Duration::from_secs(60);

/// Per-tenant token bucket, applied after resolution: the bucket is keyed by
/// tenant id, so one tenant's burst cannot starve its neighbours.
/// `None` limiter = pass-through (feature off).
#[derive(Clone)]
pub(crate) struct TenantRateLimitLayer {
    limiter: Option<Arc<RateLimiter>>,
}

impl TenantRateLimitLayer {
    pub fn new(limiter: Option<Arc<RateLimiter>>) -> Self {
        Self { limiter }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for TenantRateLimitLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = TenantRateLimitService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(TenantRateLimitService {
            service: std::rc::Rc::new(service),
            limiter: self.limiter.clone(),
        }))
    }
}

pub(crate) struct TenantRateLimitService<S> {
    service: std::rc::Rc<S>,
    limiter: Option<Arc<RateLimiter>>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for TenantRateLimitService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let svc = std::rc::Rc::clone(&self.service);
        let Some(limiter) = self.limiter.clone() else {
            return Box::pin(async move { svc.call(req).await });
        };
        Box::pin(async move {
            let Some(context) = req.extensions().get::<TenantContext>().cloned() else {
                // Public path or resolution failure: nothing to limit here.
                return svc.call(req).await;
            };

            let path = req.path().to_string();
            let request_id = req.extensions().get::<RequestId>().copied();
            let bucket_key = format!("sanvi:rl:tenant:{}", context.tenant_id);
            match limiter.try_acquire_bucket(&bucket_key).await {
                Ok(()) => svc.call(req).await,
                Err(crate::rate_limit::LimitError::Denied { retry_after_secs }) => {
                    tracing::warn!(
                        tenant_id = %context.tenant_id,
                        request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                        "request rejected: tenant rate limit exceeded",
                    );
                    let mut response = actix_web::HttpResponse::TooManyRequests().json(
                        crate::error::ApiError::TooManyRequests { retry_after_secs }
                            .with_context(Some(path), request_id.map(|id| id.to_string())),
                    );
                    response.headers_mut().insert(
                        actix_web::http::header::RETRY_AFTER,
                        actix_web::http::header::HeaderValue::from_str(
                            &retry_after_secs.to_string(),
                        )
                        .unwrap_or_else(|_| actix_web::http::header::HeaderValue::from_static("1")),
                    );
                    Ok(req.into_response(response.map_into_boxed_body()))
                }
                Err(crate::rate_limit::LimitError::BackendUnavailable) => {
                    tracing::warn!("tenant rate limit backend unavailable; failing open");
                    svc.call(req).await
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> TenantPolicyConfig {
        TenantPolicyConfig::default()
    }

    #[test]
    fn public_paths_never_require_a_tenant() {
        assert!(policy().is_public_path("/healthz"));
        assert!(policy().is_public_path("/api/v1/platform/tenants"));
        assert!(policy().is_public_path("/api/v1/public/slug-availability"));
        assert!(!policy().is_public_path("/api/v1/tenant/context"));
        assert!(!policy().is_public_path("/api/v1/billing/invoices"));
    }

    #[test]
    fn suspended_tenants_are_locked_except_on_the_allowlist() {
        let policy = policy();
        assert_eq!(
            status_for_resolution(
                &policy,
                "/api/v1/tenant/settings",
                TenantRuntimeStatus::Suspended
            ),
            Some(StatusCode::LOCKED)
        );
        assert_eq!(
            status_for_resolution(
                &policy,
                "/api/v1/billing/invoices",
                TenantRuntimeStatus::Suspended
            ),
            None
        );
        assert_eq!(
            status_for_resolution(
                &policy,
                "/api/v1/tenant/settings",
                TenantRuntimeStatus::Active
            ),
            None
        );
    }

    #[test]
    fn provisioning_and_archived_tenants_are_also_locked_except_on_the_allowlist() {
        // Only Active tenants serve traffic (phase-01 domain invariant):
        // Provisioning hasn't finished setup and Archived is read-only, so
        // both must be gated exactly like Suspended. Restoring joins them:
        // a tenant mid-restore must not serve traffic.
        let policy = policy();
        for status in [
            TenantRuntimeStatus::Provisioning,
            TenantRuntimeStatus::Archived,
            TenantRuntimeStatus::Restoring,
        ] {
            assert_eq!(
                status_for_resolution(&policy, "/api/v1/tenant/settings", status),
                Some(StatusCode::LOCKED)
            );
            assert_eq!(
                status_for_resolution(&policy, "/api/v1/billing/invoices", status),
                None
            );
        }
    }

    #[test]
    fn locked_problem_reason_matches_the_non_active_status() {
        for (status, reason) in [
            (TenantRuntimeStatus::Suspended, "suspended"),
            (TenantRuntimeStatus::Provisioning, "provisioning"),
            (TenantRuntimeStatus::Archived, "archived"),
            (TenantRuntimeStatus::Restoring, "restoring"),
        ] {
            let problem = locked_problem(status, "/api/v1/tenant/settings", None);
            assert_eq!(problem.status, StatusCode::LOCKED.as_u16());
            assert_eq!(problem.reason.as_deref(), Some(reason));
            assert!(problem.r#type.ends_with(&format!("tenant-{reason}")));
        }
    }

    #[test]
    fn resolution_source_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ResolutionSource::InternalHeader).unwrap(),
            "\"internal_header\""
        );
        assert_eq!(
            serde_json::to_string(&ResolutionSource::Subdomain).unwrap(),
            "\"subdomain\""
        );
    }
}
