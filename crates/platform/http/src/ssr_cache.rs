//! The SSR response cache (TASK-022, FR-1111; the `platform.ssr_cache`
//! flag): a keyed cache over the anonymous storefront reads, with
//! single-flight on miss.
//!
//! This is the one piece of the phase that can serve a wrong response, so
//! its defaults are all fail-closed:
//!
//! - It engages only on GET/HEAD against the public storefront prefixes,
//!   and only when the flag is on and a backing cache exists.
//! - A request carrying an `Authorization` header is never served from or
//!   written to the cache. A response carrying `Set-Cookie`, a
//!   `Cache-Control` that forbids sharing, or a non-200 status is never
//!   stored. The tests in `tests/ssr_cache_responses.rs` are the contract,
//!   not this comment.
//! - The key is [`SsrCacheKey`] — the tenant is a constructor argument, so
//!   a tenant-less key cannot exist, and locale, theme revision and device
//!   class each partition it.
//! - A miss single-flights through the render: 500 concurrent misses on a
//!   cold key render exactly once and every waiter receives the leader's
//!   bytes. The leader always returns its *own* response, so a response the
//!   cache could not store (cookies, `no-store`) never has to be shared.
//! - Any error — tenant resolution, cache read, cache write, a body that
//!   refuses to materialize — degrades to a bypass: the pre-cache
//!   behaviour.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use actix_web::{
    HttpMessage,
    body::{BoxBody, MessageBody},
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{
        Method, StatusCode,
        header::{self, HeaderValue},
    },
};
use serde::Deserialize;

use sanvi_cache::{DeviceClass, SsrCache, SsrCacheKey};

/// The resolved half of the cache key the middleware cannot derive itself:
/// which tenant a public host belongs to and what the live theme revision
/// is. `None` bypasses the cache (unknown host, resolution trouble — the
/// handler then produces whatever response it would have without the
/// cache).
pub struct SsrKeyResolution {
    pub tenant_id: sanvi_kernel::TenantId,
    pub theme_revision: i32,
}

/// The port `apps/api` implements over tenant resolution + theme
/// resolution. Cheap on the hot path: both underlying lookups are
/// cache-first.
pub trait SsrKeyResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        locale: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<SsrKeyResolution>> + Send + 'a>>;
}

/// The public storefront read prefixes the cache engages on. These are the
/// surfaces the 11.2c profile ranked: theme, locales/messages, public plans
/// and the privacy disclosures. Tenancy's own public lookups
/// (`slug-availability`, `tenant-context`) are deliberately absent — a
/// cached availability answer would outlive the registration it reported.
pub const SSR_CACHE_PREFIXES: &[&str] = &["/api/v1/public/"];

/// The coarse device partition behind [`DeviceClass`] (FR-1111): the
/// storefront serves different markup to mobile. Two classes, classified
/// without a UA-parsing dependency — the cheap substrings every mobile
/// browser sends. If the storefront ever stops varying markup by device,
/// remove the dimension rather than leave one that only multiplies entries.
pub fn device_class_from_user_agent(user_agent: Option<&str>) -> DeviceClass {
    let Some(ua) = user_agent else {
        return DeviceClass::Desktop;
    };
    let ua = ua.to_ascii_lowercase();
    let mobile_markers = [
        "iphone",
        "ipod",
        "windows phone",
        "blackberry",
        "opera mini",
        "iemobile",
    ];
    if mobile_markers.iter().any(|marker| ua.contains(marker)) {
        return DeviceClass::Mobile;
    }
    // Android tablets omit the `Mobile` token; Android phones include it.
    if ua.contains("android") && ua.contains("mobile") {
        return DeviceClass::Mobile;
    }
    DeviceClass::Desktop
}

#[derive(Debug, Default, Deserialize)]
struct HostParam {
    host: Option<String>,
}

/// The wired cache: the backing store and the key resolver.
#[derive(Clone)]
pub struct SsrCacheWiring {
    cache: SsrCache,
    resolver: Arc<dyn SsrKeyResolver>,
}

/// The layer. Clone-shared state: the cache (and through it the
/// process-wide single-flight map) is shared by every worker. `wired: None`
/// is the flag-off assembly: a pure pass-through, the pre-cache behaviour.
#[derive(Clone)]
pub struct SsrCacheLayer {
    config: crate::config::SsrCacheConfig,
    wired: Option<SsrCacheWiring>,
}

impl SsrCacheLayer {
    /// The flag-on assembly: engaged wherever the config enables it.
    pub fn new(
        config: crate::config::SsrCacheConfig,
        cache: SsrCache,
        resolver: Arc<dyn SsrKeyResolver>,
    ) -> Self {
        Self {
            config,
            wired: Some(SsrCacheWiring { cache, resolver }),
        }
    }

    /// The flag-off assembly: the layer is in the chain but never engages.
    pub fn disabled(config: crate::config::SsrCacheConfig) -> Self {
        Self {
            config,
            wired: None,
        }
    }
}

impl<S> Transform<S, ServiceRequest> for SsrCacheLayer
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = SsrCacheService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(SsrCacheService {
            service: Arc::new(service),
            config: self.config.clone(),
            wired: self.wired.clone(),
        }))
    }
}

pub struct SsrCacheService<S> {
    service: Arc<S>,
    config: crate::config::SsrCacheConfig,
    wired: Option<SsrCacheWiring>,
}

impl<S> Service<ServiceRequest> for SsrCacheService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let engage = self.config.enabled
            && matches!(*req.method(), Method::GET | Method::HEAD)
            && SSR_CACHE_PREFIXES
                .iter()
                .any(|prefix| req.path().starts_with(prefix))
            && req.headers().get(header::AUTHORIZATION).is_none();

        let wired = if engage { self.wired.clone() } else { None };
        let Some(wiring) = wired else {
            return Box::pin(self.service.call(req));
        };

        // The future owns everything it touches: the middleware state is
        // clone-shared, the inner service lives behind an Arc.
        let service = Arc::clone(&self.service);
        let config = self.config.clone();
        Box::pin(async move { render_or_pass(service, wiring, config, req).await })
    }
}

/// The cache path: hit → replay; miss → lead the render, store if storable,
/// return the leader's own response; a lost leadership race joins the
/// flight and re-reads.
async fn render_or_pass<S>(
    service: Arc<S>,
    wiring: SsrCacheWiring,
    config: crate::config::SsrCacheConfig,
    req: ServiceRequest,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error>
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    let SsrCacheWiring { cache, resolver } = &wiring;
    let Some(key) = build_key(cache, resolver.as_ref(), &req).await else {
        return service.call(req).await;
    };
    let ttl = Duration::from_secs(config.ttl_secs);
    let canonical = key.to_cache_key();

    loop {
        if let Some(entry) = cache.get(&key).await {
            let (http_req, _) = req.into_parts();
            return Ok(replay(&http_req, &entry));
        }
        let Some(_leader) = cache.single_flight().try_lead(&canonical) else {
            cache.single_flight().wait(&canonical).await;
            cache.record_join();
            continue;
        };
        cache.record_miss();
        // Leader. The second read closes the gap between the first
        // cache check and taking leadership.
        if let Some(entry) = cache.get(&key).await {
            let (http_req, _) = req.into_parts();
            return Ok(replay(&http_req, &entry));
        }

        let res = service.call(req).await?;
        let (http_req, http_res) = res.into_parts();
        let (head, storable) = capture(&http_res);
        // `try_into_bytes` is zero-copy for the full bodies every
        // storefront handler produces; a body that refuses (streaming)
        // passes through unstored.
        let body = http_res.into_body();
        match body.try_into_bytes() {
            Ok(bytes) => {
                if storable {
                    let entry = sanvi_cache::SsrEntry {
                        status: head.status,
                        headers: head.headers.clone(),
                        body: bytes.to_vec(),
                    };
                    cache.put(&key, &entry, ttl).await;
                }
                let response = rebuild(
                    &head,
                    BoxBody::new(actix_web::web::Bytes::from(bytes.to_vec())),
                );
                return Ok(ServiceResponse::new(http_req, response));
            }
            Err(body) => {
                let response = rebuild(&head, body);
                return Ok(ServiceResponse::new(http_req, response));
            }
        }
    }
}

/// The key for this request, or `None` to bypass. Every derivation
/// failure bypasses: the cache must never make a request fail.
async fn build_key(
    _cache: &SsrCache,
    resolver: &dyn SsrKeyResolver,
    req: &ServiceRequest,
) -> Option<SsrCacheKey> {
    let query = req.query_string();
    let host: Option<String> = if query.is_empty() {
        None
    } else {
        actix_web::web::Query::<HostParam>::from_query(query)
            .ok()
            .and_then(|parsed| parsed.host.clone())
    };
    let host = host.or_else(|| {
        req.headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    })?;

    // The negotiated locale partitions the key even where a handler
    // would ignore it: over-partitioning costs entries, under-
    // partitioning serves a wrong-locale response.
    let locale = req
        .extensions()
        .get::<crate::locale::LocaleContext>()
        .map(|context| context.locale.clone())
        .unwrap_or_else(|| "en".to_string());

    let device_class = device_class_from_user_agent(
        req.headers()
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    );

    let route = route_of(req.path(), query);

    let resolution = resolver.resolve(&host, &locale).await?;

    SsrCacheKey::new(
        resolution.tenant_id,
        locale,
        resolution.theme_revision,
        device_class,
        route,
    )
    .ok()
}

/// The route component of the key: path plus query, minus the `host`
/// parameter — the host *is* the tenant dimension, and keeping it would
/// multiply entries per tenant without partitioning anything.
fn route_of(path: &str, query: &str) -> String {
    if query.is_empty() {
        return path.to_string();
    }
    let filtered: Vec<&str> = query
        .split('&')
        .filter(|pair| !is_host_param(pair))
        .collect();
    if filtered.is_empty() {
        return path.to_string();
    }
    format!("{path}?{}", filtered.join("&"))
}

fn is_host_param(pair: &str) -> bool {
    pair == "host"
        || pair.starts_with("host=")
        || pair.starts_with("host%3D")
        || pair.starts_with("host%3d")
}

/// The captured response head (always — it is what the response is rebuilt
/// from) plus the storable verdict. Fail-closed on every storable
/// dimension: only a plain 200 JSON response, without `Set-Cookie` and
/// without a cache directive that forbids sharing, may be stored.
struct CapturedHead {
    status: u16,
    headers: Vec<(String, String)>,
}

fn capture(res: &actix_web::HttpResponse) -> (CapturedHead, bool) {
    let headers: Vec<(String, String)> = res
        .headers()
        .iter()
        // The response builder re-derives `content-length` from the body.
        .filter(|(name, _)| *name != header::CONTENT_LENGTH)
        .filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str().to_string(), value.to_string()))
        })
        .collect();
    let head = CapturedHead {
        status: res.status().as_u16(),
        headers,
    };

    let storable = res.status() == StatusCode::OK
        && !res.headers().contains_key(header::SET_COOKIE)
        && res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
        && !res
            .headers()
            .get(header::CACHE_CONTROL)
            .is_some_and(|value| {
                let value = value.to_str().unwrap_or_default().to_ascii_lowercase();
                ["no-store", "no-cache", "private"]
                    .iter()
                    .any(|directive| value.contains(directive))
            });

    (head, storable)
}

/// Rebuild a response from a captured head over new body bytes.
fn rebuild(head: &CapturedHead, body: BoxBody) -> actix_web::HttpResponse {
    let status = StatusCode::from_u16(head.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = actix_web::HttpResponse::build(status);
    for (name, value) in &head.headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            builder.insert_header((name, value));
        }
    }
    builder.body(body)
}

/// Rebuild a response from a stored entry.
fn replay(req: &actix_web::HttpRequest, entry: &sanvi_cache::SsrEntry) -> ServiceResponse<BoxBody> {
    let head = CapturedHead {
        status: entry.status,
        headers: entry.headers.clone(),
    };
    let response = rebuild(
        &head,
        BoxBody::new(actix_web::web::Bytes::from(entry.body.clone())),
    );
    ServiceResponse::new(req.clone(), response)
}
