//! TASK-026: plan-aware, shared API request counters with independent enforcement.
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use actix_web::{
    HttpMessage, HttpResponse, ResponseError,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
    http::{Method, StatusCode, header},
    web,
};
use sanvi_kernel::TenantId;
use sha2::{Digest, Sha256};

use crate::{ApiError, Subject, TenantContext};

/// Billing supplies the authoritative tier; client headers are never consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitPlan {
    Starter,
    Professional,
    Enterprise,
}

pub trait PlanResolver: Send + Sync {
    fn plan<'a>(
        &'a self,
        tenant: TenantId,
    ) -> Pin<Box<dyn Future<Output = Result<LimitPlan, ApiError>> + Send + 'a>>;
}

// TASK-021 docs/runbooks/capacity.md: 840 healthy req/s / 147 tenants,
// floor = 5 req/s/tenant, at 1.5 core-ms/request. This is a lab allocation,
// not a production capacity claim. Tier weights 1/4/16 and credential
// weights 1/2/4 give these one-minute budgets. Public allocations are
// 1/5 (anonymous), 1/10 (tracking), 1/25 (verification) of Starter.
const WINDOW_SECS: u64 = 60;
const STARTER_TENANT: u64 = 300;
const PUBLIC_LIMIT: u64 = 60;
const TRACKING_LIMIT: u64 = 30;
const VERIFICATION_LIMIT: u64 = 12;

impl LimitPlan {
    fn limits(self) -> (u64, u64) {
        let (tenant_weight, credential_weight) = match self {
            Self::Starter => (1, 1),
            Self::Professional => (4, 2),
            Self::Enterprise => (16, 4),
        };
        (
            STARTER_TENANT * tenant_weight,
            STARTER_TENANT * credential_weight,
        )
    }
}

#[derive(Clone, Debug)]
struct Bucket {
    key: String,
    limit: u64,
}

#[derive(Clone, Copy, Debug)]
struct Count {
    used: u64,
    limit: u64,
    reset: u64,
}

impl Count {
    fn over(self) -> bool {
        self.used > self.limit
    }

    fn stamp(self, response: &mut ServiceResponse<BoxBody>) {
        for (name, value) in [
            ("ratelimit-limit", self.limit),
            ("ratelimit-remaining", self.limit.saturating_sub(self.used)),
            ("ratelimit-reset", self.reset),
        ] {
            response.headers_mut().insert(
                header::HeaderName::from_static(name),
                header::HeaderValue::from_str(&value.to_string()).expect("unsigned integer header"),
            );
        }
    }
}

trait RequestCounter: Send + Sync {
    fn count<'a>(
        &'a self,
        buckets: &'a [Bucket],
    ) -> Pin<Box<dyn Future<Output = Result<Count, ApiError>> + Send + 'a>>;
}

/// Install as `web::Data<Arc<ApiLimits>>` even during counting-only rollout.
pub struct ApiLimits {
    counter: Arc<dyn RequestCounter>,
    plans: Arc<dyn PlanResolver>,
    enabled: bool,
}

impl ApiLimits {
    pub fn new(
        redis: Option<redis::aio::ConnectionManager>,
        plans: Arc<dyn PlanResolver>,
        enabled: bool,
    ) -> Self {
        Self {
            counter: Arc::new(RedisCounter {
                redis,
                namespace: "sanvi:api-limits".into(),
            }),
            plans,
            enabled,
        }
    }

    async fn inspect(&self, req: &ServiceRequest) -> Result<Count, ApiError> {
        // Authentication verifies the session; impersonation preserves its
        // identifier. Tenant and credential come from verified extensions.
        let subject = req.extensions().get::<Subject>().cloned();
        let tenant = req.extensions().get::<TenantContext>().cloned();
        let verified = tenant.zip(subject).filter(|(tenant, subject)| {
            subject.tenant_id == Some(tenant.tenant_id) && !subject.session_id.is_empty()
        });
        let public = req.path().starts_with("/api/v1/public/");
        let tracking = req.path() == "/api/v1/public/track";
        let verification =
            req.path().starts_with("/api/v1/tenant/domains/") && req.path().ends_with("/verify");
        let mut buckets = Vec::with_capacity(3);
        let mut plan_error = None;
        if !public && let Some((tenant, subject)) = &verified {
            let plan = match self.plans.plan(tenant.tenant_id).await {
                Ok(plan) => plan,
                Err(error) => {
                    tracing::warn!(error = %error, "API limit tier lookup unavailable");
                    plan_error = Some(error);
                    // Continue the counting path even when the plan lookup is
                    // unavailable. Enforcement reports infrastructure failure.
                    LimitPlan::Starter
                }
            };
            let (tenant_limit, credential_limit) = plan.limits();
            let credential = hex::encode(Sha256::digest(subject.session_id.as_bytes()));
            buckets.push(Bucket {
                key: format!("tenant:{}", tenant.tenant_id),
                limit: tenant_limit,
            });
            buckets.push(Bucket {
                key: format!("tenant:{}:credential:{credential}", tenant.tenant_id),
                limit: credential_limit,
            });
        }
        if public || verified.is_none() || verification {
            // Only an explicitly trusted proxy may supply a sanitized address.
            let peer = crate::client_ip(req.request())
                .map(|address| address.to_string())
                .unwrap_or_else(|| "unknown".into());
            let (surface, limit) = if tracking {
                ("tracking", TRACKING_LIMIT)
            } else if verification {
                ("verification", VERIFICATION_LIMIT)
            } else {
                ("public", PUBLIC_LIMIT)
            };
            buckets.push(Bucket {
                key: format!("{surface}:ip:{peer}"),
                limit,
            });
        }
        let count = self.counter.count(&buckets).await?;
        if self.enabled && plan_error.is_some() {
            return Err(ApiError::service_unavailable(
                "API rate-limit plan lookup is unavailable",
            ));
        }
        Ok(count)
    }
}

// Redis TIME gives every replica the same window. All tenant/credential/public
// counts increment atomically, including over-limit and counting-only requests.
// TTL bounds live keys by the active window. No replica-local fallback exists.
const LUA_COUNT: &str = r#"
local now = tonumber(redis.call('TIME')[1])
local duration = tonumber(ARGV[1])
local window = math.floor(now / duration)
local reset = duration - now % duration
local selected_used = 0
local selected_limit = 1
for i, base in ipairs(KEYS) do
  local key = base .. ':' .. window
  local used = redis.call('INCR', key)
  redis.call('EXPIRE', key, reset + 1, 'NX')
  local limit = tonumber(ARGV[i + 1])
  if i == 1 or used / limit > selected_used / selected_limit then
    selected_used = used
    selected_limit = limit
  end
end
return {selected_used, selected_limit, reset}
"#;

struct RedisCounter {
    redis: Option<redis::aio::ConnectionManager>,
    namespace: String,
}

impl RequestCounter for RedisCounter {
    fn count<'a>(
        &'a self,
        buckets: &'a [Bucket],
    ) -> Pin<Box<dyn Future<Output = Result<Count, ApiError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(redis) = &self.redis else {
                return Err(ApiError::service_unavailable(
                    "API rate counters are unavailable",
                ));
            };
            let script = redis::Script::new(LUA_COUNT);
            let mut invocation = script.prepare_invoke();
            invocation.arg(WINDOW_SECS);
            for bucket in buckets {
                invocation
                    .key(format!("{}:{}", self.namespace, bucket.key))
                    .arg(bucket.limit);
            }
            let mut redis = redis.clone();
            match tokio::time::timeout(
                Duration::from_millis(100),
                invocation.invoke_async::<(u64, u64, u64)>(&mut redis),
            )
            .await
            {
                Ok(Ok((used, limit, reset))) => Ok(Count { used, limit, reset }),
                outcome => {
                    tracing::warn!(error = ?outcome, "API rate counting unavailable");
                    Err(ApiError::service_unavailable(
                        "API rate counters are unavailable",
                    ))
                }
            }
        })
    }
}

#[derive(Clone)]
pub struct ApiLimitLayer;

impl<S> actix_web::dev::Transform<S, ServiceRequest> for ApiLimitLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = ApiLimitService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(ApiLimitService {
            service: Arc::new(service),
        }))
    }
}

pub struct ApiLimitService<S> {
    service: Arc<S>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for ApiLimitService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;
    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let limits = req
            .app_data::<web::Data<Arc<ApiLimits>>>()
            .map(|data| data.get_ref().clone());
        let service = self.service.clone();
        Box::pin(async move {
            let outer_tracking_drop = req.extensions().contains::<TrackingRateLimitDrop>();
            let Some(limits) = limits
                .filter(|_| req.path().starts_with("/api/v1/") && req.method() != Method::OPTIONS)
            else {
                if outer_tracking_drop {
                    return Ok(req.into_response(HttpResponse::Accepted().finish()));
                }
                return service.call(req).await;
            };
            let tracking = req.path() == "/api/v1/public/track";
            let count = match limits.inspect(&req).await {
                Ok(count) => count,
                Err(error) => {
                    tracing::warn!(error = %error, "API limits counting failed");
                    if limits.enabled || outer_tracking_drop {
                        // Preserve the beacon's uniform-response security
                        // contract even on Redis or billing infrastructure failure.
                        let response = if tracking {
                            HttpResponse::Accepted().finish()
                        } else {
                            error.error_response()
                        };
                        return Ok(crate::error::normalize_boundary_error(
                            req.into_response(response),
                        )
                        .await);
                    }
                    return service.call(req).await;
                }
            };
            if count.over() {
                tracing::debug!(
                    enforced = limits.enabled,
                    limit = count.limit,
                    "API rate would block"
                );
            }
            if outer_tracking_drop || (limits.enabled && count.over()) {
                if tracking {
                    return Ok(req.into_response(HttpResponse::Accepted().finish()));
                }
                let mut response = req.into_response(
                    ApiError::TooManyRequests {
                        retry_after_secs: count.reset,
                    }
                    .error_response(),
                );
                count.stamp(&mut response);
                return Ok(crate::error::normalize_boundary_error(response).await);
            }
            let mut response = service.call(req).await?;
            if limits.enabled && !tracking && response.status() != StatusCode::SERVICE_UNAVAILABLE {
                count.stamp(&mut response);
            }
            Ok(response)
        })
    }
}

/// The outer security limiter denied this beacon. Its empty response is
/// produced here so CORS, locale and security headers match accepted beacons.
pub(crate) struct TrackingRateLimitDrop;

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Plans;
    impl PlanResolver for Plans {
        fn plan<'a>(
            &'a self,
            _: TenantId,
        ) -> Pin<Box<dyn Future<Output = Result<LimitPlan, ApiError>> + Send + 'a>> {
            Box::pin(async { Ok(LimitPlan::Starter) })
        }
    }

    // Redis is the external boundary; these HTTP behavior tests substitute
    // only its count response. The Lua atomicity test below uses real Redis.
    struct Counter(AtomicU64);
    impl RequestCounter for Counter {
        fn count<'a>(
            &'a self,
            buckets: &'a [Bucket],
        ) -> Pin<Box<dyn Future<Output = Result<Count, ApiError>> + Send + 'a>> {
            Box::pin(async move {
                Ok(Count {
                    used: self.0.fetch_add(1, Ordering::SeqCst) + 1,
                    limit: buckets.iter().map(|bucket| bucket.limit).min().unwrap(),
                    reset: 60,
                })
            })
        }
    }

    fn limits(enabled: bool) -> Arc<ApiLimits> {
        Arc::new(ApiLimits {
            counter: Arc::new(Counter(AtomicU64::new(0))),
            plans: Arc::new(Plans),
            enabled,
        })
    }

    async fn correlate(
        req: ServiceRequest,
        next: actix_web::middleware::Next<BoxBody>,
    ) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
        req.extensions_mut()
            .insert(crate::middleware::RequestId(uuid::Uuid::nil()));
        next.call(req).await
    }

    #[actix_web::test]
    async fn public_flood_is_limited_with_all_backoff_headers() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(limits(true)))
                .wrap(ApiLimitLayer)
                .wrap(actix_web::middleware::from_fn(correlate))
                .route(
                    "/api/v1/public/catalog",
                    web::get().to(|| async { HttpResponse::Ok().finish() }),
                ),
        )
        .await;
        for _ in 0..60 {
            assert_eq!(
                test::call_service(
                    &app,
                    test::TestRequest::get()
                        .uri("/api/v1/public/catalog")
                        .to_request()
                )
                .await
                .status(),
                StatusCode::OK
            );
        }
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/public/catalog")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        for (name, value) in [
            ("retry-after", "60"),
            ("ratelimit-limit", "60"),
            ("ratelimit-remaining", "0"),
            ("ratelimit-reset", "60"),
        ] {
            assert_eq!(
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok()),
                Some(value)
            );
        }
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["instance"], "/api/v1/public/catalog");
        assert_eq!(body["trace_id"], uuid::Uuid::nil().to_string());
    }

    #[actix_web::test]
    async fn tracking_flood_drops_before_handler_with_uniform_accepted_response() {
        let processed = Arc::new(AtomicU64::new(0));
        let handler = processed.clone();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(limits(true)))
                .wrap(ApiLimitLayer)
                .route(
                    "/api/v1/public/track",
                    web::post().to(move || {
                        let handler = handler.clone();
                        async move {
                            handler.fetch_add(1, Ordering::SeqCst);
                            HttpResponse::Accepted().finish()
                        }
                    }),
                ),
        )
        .await;
        for _ in 0..100 {
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/api/v1/public/track")
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            assert!(!response.headers().contains_key("ratelimit-limit"));
            assert!(test::read_body(response).await.is_empty());
        }
        assert_eq!(processed.load(Ordering::SeqCst), 30);
    }

    #[actix_web::test]
    async fn unavailable_counters_and_platform_overload_are_503_never_429() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(ApiLimits::new(
                    None,
                    Arc::new(Plans),
                    true,
                ))))
                .wrap(ApiLimitLayer)
                .route(
                    "/api/v1/tenant/catalog",
                    web::get().to(|| async { HttpResponse::Ok().finish() }),
                ),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/tenant/catalog")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["instance"], "/api/v1/tenant/catalog");
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(limits(true)))
                .wrap(ApiLimitLayer)
                .route(
                    "/api/v1/tenant/catalog",
                    web::get().to(|| async {
                        ApiError::Overloaded {
                            retry_after_secs: 3,
                        }
                        .error_response()
                    }),
                ),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/tenant/catalog")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!response.headers().contains_key("ratelimit-limit"));
        assert_eq!(response.headers().get("retry-after").unwrap(), "3");
    }

    #[actix_web::test]
    async fn flags_off_keeps_counting_before_enforcement_is_enabled() {
        let counter = Arc::new(Counter(AtomicU64::new(0)));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(ApiLimits {
                    counter: counter.clone(),
                    plans: Arc::new(Plans),
                    enabled: false,
                })))
                .wrap(ApiLimitLayer)
                .route(
                    "/api/v1/public/catalog",
                    web::get().to(|| async { HttpResponse::Ok().finish() }),
                ),
        )
        .await;
        for _ in 0..61 {
            assert_eq!(
                test::call_service(
                    &app,
                    test::TestRequest::get()
                        .uri("/api/v1/public/catalog")
                        .to_request()
                )
                .await
                .status(),
                StatusCode::OK
            );
        }
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::new(ApiLimits {
                    counter,
                    plans: Arc::new(Plans),
                    enabled: true,
                })))
                .wrap(ApiLimitLayer)
                .route(
                    "/api/v1/public/catalog",
                    web::get().to(|| async { HttpResponse::Ok().finish() }),
                ),
        )
        .await;
        assert_eq!(
            test::call_service(
                &app,
                test::TestRequest::get()
                    .uri("/api/v1/public/catalog")
                    .to_request()
            )
            .await
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[actix_web::test]
    async fn tracking_drops_preserve_full_stack_cors_locale_and_security_headers() {
        use crate::locale::{
            LocaleContext, LocaleMiddleware, LocaleResolutionInput, LocaleResolver, LocaleSource,
        };

        struct English;
        impl LocaleResolver for English {
            fn resolve<'a>(
                &'a self,
                _: LocaleResolutionInput<'a>,
            ) -> Pin<Box<dyn Future<Output = LocaleContext> + Send + 'a>> {
                Box::pin(async {
                    LocaleContext {
                        locale: "en".into(),
                        timezone: "UTC".into(),
                        source: LocaleSource::PlatformDefault,
                        fallback_chain: vec!["en".into()],
                        tenant_id: None,
                    }
                })
            }
        }
        let processed = Arc::new(AtomicU64::new(0));
        let handler = processed.clone();
        let mut config = crate::ServerConfig::default();
        config.rate_limit.per_second = 1;
        config.abuse.tracking.enforce = false;
        let client = redis::Client::open(sanvi_testsupport::redis_url().await).unwrap();
        let mut redis = redis::aio::ConnectionManager::new(client).await.unwrap();
        let (now, _): (u64, u64) = redis::cmd("TIME").query_async(&mut redis).await.unwrap();
        if now % 60 >= 58 {
            tokio::time::sleep(Duration::from_secs(60 - now % 60)).await;
        }
        let namespace = sanvi_testsupport::test_stream("full-stack-tracking-flood");
        let digest = Sha256::digest(namespace.as_bytes());
        let peer = std::net::SocketAddr::new(
            std::net::IpAddr::V6(std::net::Ipv6Addr::from(
                <[u8; 16]>::try_from(&digest[..16]).unwrap(),
            )),
            1234,
        );
        let commercial = Arc::new(ApiLimits {
            counter: Arc::new(RedisCounter {
                redis: Some(redis.clone()),
                namespace,
            }),
            plans: Arc::new(Plans),
            enabled: true,
        });
        let app = test::init_service(crate::middleware::stack(
            App::new().app_data(web::Data::new(commercial)).route(
                "/api/v1/public/track",
                web::post().to(move || {
                    let handler = handler.clone();
                    async move {
                        handler.fetch_add(1, Ordering::SeqCst);
                        HttpResponse::Accepted().finish()
                    }
                }),
            ),
            config,
            Some(redis),
            None,
            None,
            None,
            Some(Arc::new(LocaleMiddleware {
                resolver: Arc::new(English),
            })),
            crate::ssr_cache::SsrCacheLayer::disabled(crate::config::SsrCacheConfig::default()),
            crate::degraded::DegradedHeaderLayer::disabled(),
            None,
        ))
        .await;
        let mut baseline = None;
        for _ in 0..100 {
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/api/v1/public/track")
                    .peer_addr(peer)
                    .insert_header((header::ORIGIN, "https://store.example"))
                    .insert_header(("x-request-id", "00000000-0000-0000-0000-000000000001"))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let mut headers: Vec<_> = response
                .headers()
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
                .collect();
            headers.sort();
            if let Some(baseline) = &baseline {
                assert_eq!(&headers, baseline);
            } else {
                baseline = Some(headers);
            }
            assert_eq!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .unwrap(),
                "https://store.example"
            );
            assert_eq!(response.headers().get("content-language").unwrap(), "en");
            assert_eq!(
                response.headers().get("x-content-type-options").unwrap(),
                "nosniff"
            );
            assert!(test::read_body(response).await.is_empty());
        }
        assert_eq!(processed.load(Ordering::SeqCst), 30);
    }

    #[actix_web::test]
    async fn tracking_outer_backend_failure_is_uniform_without_commercial_limits() {
        let mut baseline = None;
        for enabled in [false, true] {
            let processed = Arc::new(AtomicU64::new(0));
            let handler = processed.clone();
            let mut config = crate::ServerConfig::default();
            config.rate_limit.enabled = enabled;
            config.rate_limit.fail_open = false;
            let app = test::init_service(crate::middleware::stack(
                App::new().route(
                    "/api/v1/public/track",
                    web::post().to(move || {
                        let handler = handler.clone();
                        async move {
                            handler.fetch_add(1, Ordering::SeqCst);
                            HttpResponse::Accepted().finish()
                        }
                    }),
                ),
                config,
                None,
                None,
                None,
                None,
                None,
                crate::ssr_cache::SsrCacheLayer::disabled(crate::config::SsrCacheConfig::default()),
                crate::degraded::DegradedHeaderLayer::disabled(),
                None,
            ))
            .await;
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/api/v1/public/track")
                    .insert_header((header::ORIGIN, "https://store.example"))
                    .insert_header(("x-request-id", uuid::Uuid::nil().to_string()))
                    .to_request(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let mut headers: Vec<_> = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    let mut value = value.to_str().unwrap().to_string();
                    // Separate Apps randomize the CORS header-name set.
                    // Compare that header's semantic set across instances.
                    if name.as_str() == "access-control-expose-headers" {
                        let mut names: Vec<_> = value.split(',').map(str::trim).collect();
                        names.sort_unstable();
                        value = names.join(", ");
                    }
                    (name.to_string(), value)
                })
                .collect();
            headers.sort();
            if let Some(baseline) = &baseline {
                assert_eq!(&headers, baseline);
            } else {
                baseline = Some(headers);
            }
            assert!(test::read_body(response).await.is_empty());
            assert_eq!(
                processed.load(Ordering::SeqCst),
                if enabled { 0 } else { 1 }
            );
        }
    }

    #[actix_web::test]
    async fn domain_verification_has_a_stricter_bucket_than_anonymous_reads() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(limits(true)))
                .wrap(ApiLimitLayer)
                .route(
                    "/api/v1/tenant/domains/{id}/verify",
                    web::post().to(|| async { HttpResponse::Accepted().finish() }),
                ),
        )
        .await;
        for _ in 0..12 {
            assert_eq!(
                test::call_service(
                    &app,
                    test::TestRequest::post()
                        .uri("/api/v1/tenant/domains/7/verify")
                        .to_request()
                )
                .await
                .status(),
                StatusCode::ACCEPTED
            );
        }
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/v1/tenant/domains/8/verify")
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("ratelimit-limit").unwrap(), "12");
    }

    #[actix_web::test]
    async fn authoritative_tier_enforces_credential_and_tenant_buckets_independently() {
        use std::{collections::HashMap, sync::Mutex};
        struct Professional;
        impl PlanResolver for Professional {
            fn plan<'a>(
                &'a self,
                _: TenantId,
            ) -> Pin<Box<dyn Future<Output = Result<LimitPlan, ApiError>> + Send + 'a>>
            {
                Box::pin(async { Ok(LimitPlan::Professional) })
            }
        }
        struct KeyedCounter(Mutex<HashMap<String, u64>>);
        impl RequestCounter for KeyedCounter {
            fn count<'a>(
                &'a self,
                buckets: &'a [Bucket],
            ) -> Pin<Box<dyn Future<Output = Result<Count, ApiError>> + Send + 'a>> {
                Box::pin(async move {
                    let mut counts = self.0.lock().unwrap();
                    let mut selected = Count {
                        used: 0,
                        limit: 1,
                        reset: 60,
                    };
                    for bucket in buckets {
                        let used = counts.entry(bucket.key.clone()).or_default();
                        *used += 1;
                        if *used * selected.limit > selected.used * bucket.limit {
                            selected = Count {
                                used: *used,
                                limit: bucket.limit,
                                reset: 60,
                            };
                        }
                    }
                    Ok(selected)
                })
            }
        }
        fn verified(tenant: i64, session: &str) -> ServiceRequest {
            let request = test::TestRequest::get()
                .uri("/api/v1/tenant/catalog")
                .insert_header(("x-plan", "enterprise"))
                .insert_header(("x-api-key", "forged"))
                .to_srv_request();
            let tenant_id = TenantId::try_from(tenant).unwrap();
            request.extensions_mut().insert(TenantContext {
                tenant_id,
                slug: "test".into(),
                display_name: "Test".into(),
                status: crate::TenantRuntimeStatus::Active,
                region: "jp".into(),
                default_locale: "en".into(),
                resolution_source: crate::ResolutionSource::SessionClaim,
            });
            request.extensions_mut().insert(Subject {
                user_id: sanvi_kernel::UserId::try_from(5_i64).unwrap(),
                session_id: session.into(),
                aal: "aal1".into(),
                methods: vec![],
                tenant_id: Some(tenant_id),
                role_ids: vec![],
                authenticated_at: None,
            });
            request
        }
        let limits = ApiLimits {
            counter: Arc::new(KeyedCounter(Mutex::new(HashMap::new()))),
            plans: Arc::new(Professional),
            enabled: true,
        };
        for _ in 0..600 {
            assert!(
                !limits
                    .inspect(&verified(7, "session-a"))
                    .await
                    .unwrap()
                    .over()
            );
        }
        let credential = limits.inspect(&verified(7, "session-a")).await.unwrap();
        assert!(credential.over());
        assert_eq!(credential.limit, 600);
        for _ in 0..599 {
            assert!(
                !limits
                    .inspect(&verified(7, "session-b"))
                    .await
                    .unwrap()
                    .over()
            );
        }
        let tenant = limits.inspect(&verified(7, "session-b")).await.unwrap();
        assert!(tenant.over());
        assert_eq!(tenant.limit, 1200);
        assert!(
            !limits
                .inspect(&verified(8, "session-a"))
                .await
                .unwrap()
                .over()
        );
    }

    #[tokio::test]
    async fn redis_counting_is_atomic_across_replicas_and_survives_a_counter_restart() {
        let client = redis::Client::open(sanvi_testsupport::redis_url().await).unwrap();
        let mut redis = redis::aio::ConnectionManager::new(client).await.unwrap();
        let (now, _): (u64, u64) = redis::cmd("TIME").query_async(&mut redis).await.unwrap();
        // This test intentionally observes a single window. Avoid the last
        // two seconds rather than making a minute-seam admission assertion.
        if now % 60 >= 58 {
            tokio::time::sleep(Duration::from_secs(60 - now % 60)).await;
        }
        let namespace = sanvi_testsupport::test_stream("api-rates-replicas");
        let first = RedisCounter {
            redis: Some(redis.clone()),
            namespace: namespace.clone(),
        };
        let second = RedisCounter {
            redis: Some(redis.clone()),
            namespace: namespace.clone(),
        };
        let buckets = [
            Bucket {
                key: "tenant:7".into(),
                limit: 60,
            },
            Bucket {
                key: "tenant:7:credential:verified".into(),
                limit: 30,
            },
        ];
        let counts = futures_util::future::join_all((0..40).map(|index| {
            let counter = if index % 2 == 0 { &first } else { &second };
            counter.count(&buckets)
        }))
        .await;
        let mut used: Vec<_> = counts
            .into_iter()
            .map(|count| count.unwrap().used)
            .collect();
        used.sort_unstable();
        assert_eq!(used, (1..=40).collect::<Vec<_>>());
        let restarted = RedisCounter {
            redis: Some(redis),
            namespace,
        };
        let count = restarted.count(&buckets).await.unwrap();
        assert_eq!(count.used, 41);
        assert_eq!(count.limit, 30);
        assert!((1..=60).contains(&count.reset));
    }
}
