use std::{sync::Arc, time::Duration};

use actix_web::{
    HttpMessage, HttpResponse, ResponseError,
    body::{BoxBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    http::header,
};
use futures_util::FutureExt as _;
use uuid::Uuid;

use crate::authn::AuthnLayer;
use crate::compression::SelectiveCompression;
use crate::config::{CorsConfig, RateLimitConfig, ServerConfig};
use crate::etag::ConditionalEtag;
use crate::impersonation::ImpersonationLayer;
use crate::locale::{LocaleMiddleware, LocaleResolutionLayer};
use crate::metrics::{HttpInstruments, context_from_path};
use crate::rate_limit::RateLimiter;
use crate::tenancy::{TenantMiddleware, TenantRateLimitLayer, TenantResolutionLayer};

/// Request correlation id; present on every request, in logs, traces and the
/// `x-request-id` response header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub Uuid);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

const REQUEST_ID_HEADER: &str = "x-request-id";

/// Header carrying the origin-bound site key that authenticates the public
/// tracking beacon.
const SITE_KEY_HEADER: &str = "x-site-key";

/// Paths served to arbitrary third-party origins rather than to the
/// platform's own front ends, and therefore exempt from
/// [`CorsConfig::allowed_origins`].
///
/// `/public/track` is posted from every tenant's storefront. Those origins
/// are tenant-owned and unbounded, so they cannot be enumerated in a
/// platform-wide allow-list; the request is authenticated by a site key
/// bound to the very `Origin` header being checked here, which is a
/// stronger check than the list would have been. Its response is an empty
/// `202` in every case, so reflecting the caller's origin discloses
/// nothing.
const PUBLIC_CROSS_ORIGIN_PATHS: &[&str] = &["/api/v1/public/track"];

/// Apply the canonical middleware chain to an `App`.
///
/// Outermost → innermost:
/// metrics → request id → tracing span → traceparent header → concurrency
/// limit → panic catch → rate limit → tenant resolution → authentication →
/// impersonation → locale negotiation → tenant rate limit → request timeout
/// → body normalize → CORS → abuse quota → compression → etag → security
/// headers → error handlers.
///
/// `tenant` is `None` until the tenancy context is wired (phase 01+); without
/// it every route behaves as a public route. `authn` is `None` until the
/// identity context is wired (phase 02+); the extractors then treat every
/// route as unauthenticated. `impersonation` is `None` until the
/// platform_admin context is wired (phase 03+); it runs immediately after
/// authentication so the *effective* subject is what permission checks and
/// handlers see. `locale` is `None` until the localization context is wired
/// (phase 06+); it runs after impersonation so negotiation sees the effective
/// subject and tenant, and before the error handlers so problem bodies are
/// localized. `load_shed` is the process-global shedding state built once by
/// the composition root (TASK-023); `None` (most tests) is a pass-through.
/// The shedder sits inside tenant resolution so per-tenant fairness can read
/// the resolved tenant. The outer per-IP limiter observes Redis health;
/// the inner tenant limiter runs only after admission.
///
/// The tracing span wraps every protective middleware, so one span covers a
/// request from accept to first byte; `tracing-actix-web` maps response
/// bodies to its streaming type as the outermost body transform.
#[allow(clippy::too_many_arguments)]
pub fn stack<T>(
    app: actix_web::App<T>,
    config: ServerConfig,
    redis: Option<redis::aio::ConnectionManager>,
    tenant: Option<std::sync::Arc<TenantMiddleware>>,
    authn: Option<std::sync::Arc<crate::authn::AuthnMiddleware>>,
    impersonation: Option<std::sync::Arc<crate::impersonation::ImpersonationMiddleware>>,
    locale: Option<std::sync::Arc<LocaleMiddleware>>,
    ssr_cache: crate::ssr_cache::SsrCacheLayer,
    degraded: crate::degraded::DegradedHeaderLayer,
    load_shed: Option<std::sync::Arc<crate::load_shed::LoadShedState>>,
) -> actix_web::App<
    impl actix_web::dev::ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<tracing_actix_web::StreamSpan<BoxBody>>,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
>
where
    T: actix_web::dev::ServiceFactory<
            ServiceRequest,
            Config = (),
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
            InitError = (),
        > + 'static,
{
    let cors = build_cors(&config.cors);
    let degradation = load_shed.as_ref().and_then(|state| state.degradation());
    let observe = |limiter: RateLimiter| match &degradation {
        Some(registry) => limiter.with_degradation(registry.clone()),
        None => limiter,
    };
    let limiter = Arc::new(observe(RateLimiter::new(&config.rate_limit, redis.clone())));
    // The directive is digits and fixed ASCII by construction, so rendering
    // it cannot fail; HSTS is enabled by the staging/production overlays
    // and absent in plain-HTTP local development.
    let hsts = config.security.hsts.as_ref().map(|hsts| {
        header::HeaderValue::from_str(&hsts.header_value())
            .expect("hsts directive renders to a valid header value")
    });

    // The per-tenant limiter uses its own bucket namespace but the same
    // Redis backend as the per-IP limiter.
    let tenant_limiter = tenant
        .as_ref()
        .and_then(|parts| parts.policy.tenant_rate_limit.as_ref())
        .filter(|rate_config| rate_config.enabled)
        .map(|rate_config| Arc::new(observe(RateLimiter::new(rate_config, redis.clone()))));

    app
        .wrap(crate::error::ErrorHandlersLayer)
        // Per-app cache headers for responses reaching the inner stack.
        // Degraded headers are stamped outside the protective layers below.
        .wrap(crate::cache_headers::CacheHeadersLayer::new(
            config.cache_headers.clone(),
        ))
        .wrap(SecurityHeaders { hsts })
        // Validators over the identity bytes, before compression encodes
        // them; compression outside the ETag so a 304 never carries a
        // `Content-Encoding` and validators stay encoding-independent.
        .wrap(ConditionalEtag)
        .wrap(SelectiveCompression::new(config.compression))
        // Inside the security headers on purpose: a beacon the abuse quota
        // drops must be indistinguishable from one the handler accepted —
        // same status, body, and header set — or the headers become the
        // existence oracle the uniform 202 exists to deny. Tenant context
        // is already in the request extensions at this depth.
        .wrap(crate::abuse::AbuseQuotaLayer::new(
            std::sync::Arc::new(crate::abuse::AbuseCounter::new(redis.clone())),
            config.abuse.clone(),
        ))
        .wrap(cors)
        .wrap(NormalizeBody)
        .wrap(RequestTimeout {
            duration: Duration::from_secs(config.request_timeout_secs),
        })
        .wrap(TenantRateLimitLayer::new(tenant_limiter))
        .wrap(LocaleResolutionLayer::new(locale))
        // The keyed SSR cache (TASK-022): inside the locale middleware —
        // the negotiated locale partitions the key — and outside
        // authentication, whose credentials the cache must never see. The
        // flag-off assembly is a pass-through.
        .wrap(ssr_cache)
        .wrap(ImpersonationLayer::new(impersonation))
        .wrap(AuthnLayer::new(authn))
        .wrap(crate::load_shed::LoadShedding::new(load_shed))
        .wrap(TenantResolutionLayer::new(tenant))
        .wrap(RateLimit {
            limiter,
            config: config.rate_limit.clone(),
        })
        .wrap(CatchPanic)
        .wrap(ConcurrencyLimit::new(config.max_concurrency))
        // Actix runs the last registered wrapper first. Stamp after all
        // short-circuiting protective layers and error normalization.
        .wrap(degraded)
        // Inside the request span: the traceparent response header and the
        // error bodies' trace_id must name *this server's* span, which only
        // exists while `TracingLogger` holds it entered. Wrapping after it
        // would read an exited span and stamp nothing.
        .wrap(crate::trace_context::TraceparentLayer)
        .wrap(crate::tracing::TracingLogger::<
            crate::tracing::SanviRootSpanBuilder,
        >::new())
        .wrap(RequestIdLayer)
        .wrap(RecordMetrics)
}

fn build_cors(config: &CorsConfig) -> actix_cors::Cors {
    let origins = config.allowed_origins.clone();
    actix_cors::Cors::default()
        .allowed_origin_fn(move |origin, req_head| {
            // The preflight carries the same path as the request it
            // precedes, so one check covers both.
            if PUBLIC_CROSS_ORIGIN_PATHS.contains(&req_head.uri.path()) {
                return true;
            }
            // Explicit allow-list only; actix-cors's origin matcher parses and
            // compares origins exactly — wildcards are rejected by config
            // validation.
            origins
                .iter()
                .any(|allowed| allowed == origin.to_str().unwrap_or_default())
        })
        .allowed_methods(["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"])
        .allowed_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            header::ACCEPT,
            header::ORIGIN,
            header::HeaderName::from_static(REQUEST_ID_HEADER),
            header::HeaderName::from_static(SITE_KEY_HEADER),
            // Sent by @sanvi/api-client on every tenant-scoped request and
            // on localized/idempotent calls — a preflight requesting a
            // header missing here fails, so these must match the client.
            header::HeaderName::from_static("x-tenant-id"),
            header::HeaderName::from_static("idempotency-key"),
            header::ACCEPT_LANGUAGE,
            // Sent by @sanvi/api-client on *every* request (TASK-020 trace
            // context) — a preflight requesting a header missing here fails,
            // so omitting these breaks all cross-origin browser calls.
            header::HeaderName::from_static("traceparent"),
            header::HeaderName::from_static("tracestate"),
        ])
        .max_age(config.max_age_secs as usize)
        .supports_credentials()
}

#[cfg(test)]
mod cors_tests {
    use actix_web::{App, HttpResponse, http::header, test, web};

    use super::*;

    fn cors() -> actix_cors::Cors {
        build_cors(&CorsConfig {
            allowed_origins: vec!["https://console.example.com".to_string()],
            allow_credentials: true,
            max_age_secs: 600,
        })
    }

    async fn preflight(path: &str, origin: &str, request_headers: &str) -> u16 {
        let app = test::init_service(
            App::new().wrap(cors()).service(
                web::resource(path)
                    .route(web::post().to(|| async { HttpResponse::Accepted().finish() })),
            ),
        )
        .await;
        let request = test::TestRequest::default()
            .method(actix_web::http::Method::OPTIONS)
            .uri(path)
            .insert_header((header::ORIGIN, origin))
            .insert_header((header::ACCESS_CONTROL_REQUEST_METHOD, "POST"))
            .insert_header((header::ACCESS_CONTROL_REQUEST_HEADERS, request_headers))
            .to_request();
        test::call_service(&app, request).await.status().as_u16()
    }

    /// The beacon is posted from tenant-owned storefronts that can never be
    /// in the platform allow-list, with a custom header that forces a
    /// preflight. If either check rejects it, no browser ever reaches the
    /// handler.
    #[actix_web::test]
    async fn the_public_beacon_preflight_passes_from_an_unlisted_tenant_origin() {
        assert_eq!(
            preflight(
                "/api/v1/public/track",
                "https://shop.tenant.example",
                "content-type,x-site-key"
            )
            .await,
            200
        );
    }

    /// The exemption is path-scoped: everything else still answers only to
    /// the configured origins.
    #[actix_web::test]
    async fn other_paths_still_reject_an_unlisted_origin() {
        assert_ne!(
            preflight(
                "/api/v1/tenant/ads/conversions",
                "https://shop.tenant.example",
                "content-type"
            )
            .await,
            200
        );
        assert_eq!(
            preflight(
                "/api/v1/tenant/ads/conversions",
                "https://console.example.com",
                "content-type"
            )
            .await,
            200
        );
    }

    /// `@sanvi/api-client` sends a W3C `traceparent` on every request
    /// (TASK-020 trace context); a preflight that requests it must pass, or
    /// every cross-origin browser call dies before reaching a handler.
    #[actix_web::test]
    async fn the_trace_context_headers_pass_preflight() {
        assert_eq!(
            preflight(
                "/api/v1/tenant/ads/conversions",
                "https://console.example.com",
                "content-type,authorization,traceparent,tracestate"
            )
            .await,
            200
        );
    }
}

// ---------------------------------------------------------------------------
// Metrics (outermost): RED metrics for every response, including the ones
// produced by the protective middlewares below.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RecordMetrics;

impl<S, B> actix_web::dev::Transform<S, ServiceRequest> for RecordMetrics
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = RecordMetricsService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(RecordMetricsService { service }))
    }
}

struct RecordMetricsService<S> {
    service: S,
}

impl<S, B> actix_web::dev::Service<ServiceRequest> for RecordMetricsService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let started = std::time::Instant::now();
        let fut = self.service.call(req);
        Box::pin(async move {
            let outcome = fut.await;
            let duration = started.elapsed();
            let instruments = HttpInstruments::process();

            match &outcome {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let request = response.request();
                    let method = request.method().as_str().to_string();
                    let route = request
                        .match_pattern()
                        .unwrap_or_else(|| request.path().to_string());
                    let context = context_from_path(request.path()).to_string();
                    instruments.record(&method, &route, &context, status, duration);
                }
                Err(err) => {
                    let status = err.as_response_error().status_code().as_u16();
                    instruments.record("UNKNOWN", "unmatched", "platform", status, duration);
                }
            }
            outcome
        })
    }
}

// ---------------------------------------------------------------------------
// Concurrency limit: hard cap on in-flight requests.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ConcurrencyLimit {
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl ConcurrencyLimit {
    fn new(limit: usize) -> Self {
        Self {
            semaphore: Arc::new(tokio::sync::Semaphore::new(limit)),
        }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for ConcurrencyLimit
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = ConcurrencyLimitService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(ConcurrencyLimitService {
            service,
            semaphore: self.semaphore.clone(),
        }))
    }
}

struct ConcurrencyLimitService<S> {
    service: S,
    semaphore: Arc<tokio::sync::Semaphore>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for ConcurrencyLimitService<S>
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
        let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let request_id = req.extensions().get::<RequestId>().copied();
                let path = req.path().to_string();
                tracing::warn!(
                    request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                    "request rejected: concurrency limit reached"
                );
                let problem = crate::error::ApiError::ServiceUnavailable {
                    detail: "server is at capacity, retry later".to_string(),
                }
                .with_context(Some(path), request_id.map(|id| id.to_string()));
                let mut res = HttpResponse::ServiceUnavailable().json(problem);
                res.headers_mut()
                    .insert(header::RETRY_AFTER, header::HeaderValue::from_static("5"));
                return Box::pin(async move { Ok(req.into_response(res.map_into_boxed_body())) });
            }
        };
        let fut = self.service.call(req);
        Box::pin(async move {
            let result = fut.await;
            drop(permit);
            result
        })
    }
}

// ---------------------------------------------------------------------------
// Request id: assign or propagate a correlation id.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RequestIdLayer;

impl<S, B> actix_web::dev::Transform<S, ServiceRequest> for RequestIdLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = RequestIdService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(RequestIdService { service }))
    }
}

struct RequestIdService<S> {
    service: S,
}

impl<S, B> actix_web::dev::Service<ServiceRequest> for RequestIdService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let inbound = req
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<Uuid>().ok());

        let request_id = RequestId(inbound.unwrap_or_else(Uuid::now_v7));
        req.extensions_mut().insert(request_id);

        let fut = self.service.call(req);
        Box::pin(async move {
            let mut res = fut.await?;
            res.headers_mut().insert(
                header::HeaderName::from_static(REQUEST_ID_HEADER),
                header::HeaderValue::from_str(&request_id.to_string())
                    .unwrap_or_else(|_| header::HeaderValue::from_static("invalid")),
            );
            Ok(res)
        })
    }
}

// ---------------------------------------------------------------------------
// Panic catch: 500 problem+json without leaking internals.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CatchPanic;

impl<S> actix_web::dev::Transform<S, ServiceRequest> for CatchPanic
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = CatchPanicService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(CatchPanicService { service }))
    }
}

struct CatchPanicService<S> {
    service: S,
}

impl<S> actix_web::dev::Service<ServiceRequest> for CatchPanicService<S>
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
        let request_id = req.extensions().get::<RequestId>().copied();
        let path = req.path().to_string();
        let fut = self.service.call(req);
        Box::pin(async move {
            match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
                Ok(result) => result,
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    tracing::error!(
                        error = %message,
                        path = %path,
                        request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                        "request handler panicked"
                    );
                    Err(actix_web::Error::from(crate::error::ApiError::Internal {
                        detail: "unexpected error while processing the request".to_string(),
                    }))
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Rate limit: per-IP Redis token bucket.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RateLimit {
    limiter: Arc<RateLimiter>,
    config: RateLimitConfig,
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for RateLimit
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = RateLimitService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(RateLimitService {
            service: std::rc::Rc::new(service),
            limiter: self.limiter.clone(),
            config: self.config.clone(),
        }))
    }
}

struct RateLimitService<S> {
    service: std::rc::Rc<S>,
    limiter: Arc<RateLimiter>,
    config: RateLimitConfig,
}

impl<S> actix_web::dev::Service<ServiceRequest> for RateLimitService<S>
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
        let limiter = self.limiter.clone();
        let config = self.config.clone();
        Box::pin(async move {
            let path = req.path().to_string();
            if !config.enabled || config.exempt_paths.contains(&path) {
                return svc.call(req).await;
            }

            let ip = req
                .connection_info()
                .realip_remote_addr()
                .map(str::to_string)
                .unwrap_or_else(|| "unknown".to_string());
            let request_id = req.extensions().get::<RequestId>().copied();

            match limiter.try_acquire(&ip).await {
                Ok(()) => svc.call(req).await,
                Err(crate::rate_limit::LimitError::Denied { retry_after_secs }) => {
                    tracing::warn!(
                        client_ip = %ip,
                        request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                        "request rejected: rate limit exceeded"
                    );
                    let mut response = HttpResponse::TooManyRequests().json(
                        crate::error::ApiError::TooManyRequests { retry_after_secs }
                            .with_context(Some(path), request_id.map(|id| id.to_string())),
                    );
                    response.headers_mut().insert(
                        header::RETRY_AFTER,
                        header::HeaderValue::from_str(&retry_after_secs.to_string())
                            .unwrap_or_else(|_| header::HeaderValue::from_static("1")),
                    );
                    Ok(req.into_response(response.map_into_boxed_body()))
                }
                Err(crate::rate_limit::LimitError::BackendUnavailable) if config.fail_open => {
                    tracing::warn!("rate limit backend unavailable; failing open");
                    svc.call(req).await
                }
                Err(crate::rate_limit::LimitError::BackendUnavailable) => Ok(req.into_response(
                    crate::error::ApiError::service_unavailable("rate limit backend unavailable")
                        .error_response()
                        .map_into_boxed_body(),
                )),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Request timeout.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RequestTimeout {
    duration: Duration,
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for RequestTimeout
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = RequestTimeoutService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(RequestTimeoutService {
            service,
            duration: self.duration,
        }))
    }
}

struct RequestTimeoutService<S> {
    service: S,
    duration: Duration,
}

impl<S> actix_web::dev::Service<ServiceRequest> for RequestTimeoutService<S>
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
        let request_id = req.extensions().get::<RequestId>().copied();
        let path = req.path().to_string();
        let duration = self.duration;
        let fut = self.service.call(req);
        Box::pin(async move {
            match tokio::time::timeout(duration, fut).await {
                Ok(result) => result,
                Err(_) => {
                    tracing::warn!(
                        path = %path,
                        request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                        "request timed out"
                    );
                    Err(actix_web::Error::from(
                        crate::error::ApiError::RequestTimeout,
                    ))
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Body normalization: CORS wraps responses in `EitherBody`; this unwraps both
// variants back into the chain's body type.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct NormalizeBody;

impl<S> actix_web::dev::Transform<S, ServiceRequest> for NormalizeBody
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<actix_http::body::EitherBody<BoxBody, BoxBody>>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = NormalizeBodyService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(NormalizeBodyService { service }))
    }
}

struct NormalizeBodyService<S> {
    service: S,
}

impl<S> actix_web::dev::Service<ServiceRequest> for NormalizeBodyService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<actix_http::body::EitherBody<BoxBody, BoxBody>>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        use actix_http::body::EitherBody;
        let fut = self.service.call(req);
        Box::pin(async move {
            let res = fut.await?;
            Ok(res.map_body(|_, either| match either {
                EitherBody::Left { body } | EitherBody::Right { body } => body,
            }))
        })
    }
}

// ---------------------------------------------------------------------------
// Security headers (NFR-1114): the specified set, pinned here and asserted
// per serving surface by `apps/api/tests/integration/headers_snapshot.rs`.
// Every value is a code constant on purpose — the set has no per-environment
// variance, and making it configurable would invite weakening it quietly.
// HSTS is the one exception: it varies (TLS versus local HTTP), so it comes
// from `server.security.hsts` rather than a host-name heuristic.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SecurityHeaders {
    /// Precomputed `Strict-Transport-Security` value; `None` emits no header.
    hsts: Option<header::HeaderValue>,
}

impl<S, B> actix_web::dev::Transform<S, ServiceRequest> for SecurityHeaders
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Transform = SecurityHeadersService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(SecurityHeadersService {
            service,
            hsts: self.hsts.clone(),
        }))
    }
}

struct SecurityHeadersService<S> {
    service: S,
    hsts: Option<header::HeaderValue>,
}

impl<S, B> actix_web::dev::Service<ServiceRequest> for SecurityHeadersService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<B>,
            Error = actix_web::Error,
        > + 'static,
    B: MessageBody + 'static,
{
    type Response = ServiceResponse<B>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let hsts = self.hsts.clone();
        let fut = self.service.call(req);
        Box::pin(async move {
            let mut res = fut.await?;
            let headers = res.headers_mut();
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                header::HeaderValue::from_static("nosniff"),
            );
            headers.insert(
                header::REFERRER_POLICY,
                header::HeaderValue::from_static("same-origin"),
            );
            headers.insert(
                header::X_FRAME_OPTIONS,
                header::HeaderValue::from_static("DENY"),
            );
            headers.insert(
                header::HeaderName::from_static("permissions-policy"),
                header::HeaderValue::from_static(
                    "camera=(), microphone=(), geolocation=(), interest-cohort=()",
                ),
            );
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                header::HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
            );
            if let Some(value) = hsts {
                headers.insert(header::STRICT_TRANSPORT_SECURITY, value);
            }
            Ok(res)
        })
    }
}

#[cfg(test)]
mod security_headers_tests {
    use actix_web::{App, HttpResponse, test, web};

    use super::*;

    async fn response_headers(
        hsts: Option<&crate::config::HstsConfig>,
    ) -> actix_web::dev::ServiceResponse<BoxBody> {
        let hsts_value = hsts.map(|hsts| {
            header::HeaderValue::from_str(&hsts.header_value())
                .expect("hsts directive renders to a valid header value")
        });
        let app = test::init_service(App::new().wrap(SecurityHeaders { hsts: hsts_value }).route(
            "/probe",
            web::get().to(|| async { HttpResponse::Ok().json(serde_json::json!({})) }),
        ))
        .await;
        let req = test::TestRequest::get().uri("/probe").to_request();
        test::call_service(&app, req).await
    }

    /// The five fixed headers, exactly; nothing else security-shaped may
    /// appear or disappear unnoticed. The full per-surface snapshot lives
    /// in `apps/api/tests/integration/headers_snapshot.rs`; this is the
    /// unit-level pin of the middleware itself.
    #[actix_web::test]
    async fn the_five_fixed_headers_are_exactly_as_specified() {
        let res = response_headers(None).await;
        assert_eq!(res.status().as_u16(), 200);
        let observed: Vec<(String, String)> = [
            "x-content-type-options",
            "referrer-policy",
            "x-frame-options",
            "permissions-policy",
            "content-security-policy",
            "strict-transport-security",
        ]
        .iter()
        .filter_map(|name| {
            res.headers()
                .get(*name)
                .map(|value| (name.to_string(), value.to_str().unwrap().to_string()))
        })
        .collect();
        assert_eq!(
            observed,
            vec![
                ("x-content-type-options".to_string(), "nosniff".to_string()),
                ("referrer-policy".to_string(), "same-origin".to_string()),
                ("x-frame-options".to_string(), "DENY".to_string()),
                (
                    "permissions-policy".to_string(),
                    "camera=(), microphone=(), geolocation=(), interest-cohort=()".to_string()
                ),
                (
                    "content-security-policy".to_string(),
                    "default-src 'none'; frame-ancestors 'none'".to_string()
                ),
            ]
        );
        // No framework fingerprint headers either.
        assert!(res.headers().get("server").is_none());
        assert!(res.headers().get("x-powered-by").is_none());
    }

    /// Local development posture: no TLS, no HSTS header at all — a browser
    /// must not cache a year-long HTTPS-only rule for a loopback host.
    #[actix_web::test]
    async fn without_hsts_config_the_header_is_absent() {
        let res = response_headers(None).await;
        assert!(res.headers().get("strict-transport-security").is_none());
    }

    /// The staging/production posture: the configured directive verbatim.
    #[actix_web::test]
    async fn hsts_config_renders_the_directive_verbatim() {
        let res = response_headers(Some(&crate::config::HstsConfig {
            max_age_secs: 31_536_000,
            include_sub_domains: true,
            preload: false,
        }))
        .await;
        assert_eq!(
            res.headers()
                .get("strict-transport-security")
                .unwrap()
                .to_str()
                .unwrap(),
            "max-age=31536000; includeSubDomains"
        );
    }
}
