//! Abuse controls (TASK-024, FR-1117): per-tenant and per-IP fixed-window
//! quotas on the surfaces that cost money or reputation.
//!
//! Two knobs are deliberately separate:
//!
//! - **Counting is always live.** Every configured surface counts its
//!   requests (or, for notification sends, its sends) and records a
//!   `would_block` metric when a quota would have been exceeded, whether or
//!   not anything is enforced. Thresholds cannot be tuned without traffic,
//!   so they ship in configuration and tuning is a config change later
//!   rather than a code change (recorded in `docs/release/needs-humans.md`).
//! - **Enforcement is off for every surface except the public tracking
//!   beacon.** A denied shadow request passes through untouched; a denied
//!   enforced one is answered before the handler. The beacon already
//!   answers an indistinguishable empty `202` to everything, and its abuse
//!   quota keeps that contract: the deny response is the same `202` the
//!   handler would have produced, and the middleware sits *inside* the
//!   security-headers layer so even the response headers cannot tell a
//!   blocked beacon from an accepted one. Every other surface denies with
//!   the platform's standard `429` problem+json and `Retry-After` — none of
//!   them ship with `enforce = true` yet, so that path is exercised by
//!   tests only until traffic justifies flipping it.
//!
//! The counter is a fixed window, not a token bucket: "N invitations per
//! hour" is the shape the requirement names, the whole decision is one
//! `INCR`, and the window edge is the honest cost (a burst at a window
//! boundary admits up to 2× `limit` across the seam). Redis is the
//! authoritative store and the key namespace is shared by every API
//! instance; when Redis is not configured (local development) or a check
//! errors mid-flight, counting falls back to an in-process window rather
//! than stopping — shadow data should not depend on infrastructure health,
//! and an enforced quota that failed closed would let anyone take the
//! beacon down by killing Redis.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use actix_web::{
    HttpMessage, HttpResponse,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
    http::{Method, header},
};

use crate::RequestId;

/// Fixed-window counting in Redis: one atomic `INCR`, with the TTL set only
/// when the key has none (`EXPIRE … NX`) so a crash between the two can
/// never leave an immortal key. Redis 7+ (the compose stack and the test
/// harness both pin Redis 8).
const LUA_FIXED_WINDOW: &str = r#"
local n = redis.call('INCR', KEYS[1])
redis.call('EXPIRE', KEYS[1], tonumber(ARGV[1]), 'NX')
return n
"#;

/// Ceiling on distinct keys the in-process fallback holds. IP and Origin
/// are attacker-chosen on the public beacon, so the table must stay bounded
/// the same way the advertising quota limiter's is.
const MAX_LOCAL_KEYS: usize = 8192;

/// What one counted event resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaVerdict {
    WithinQuota,
    OverQuota {
        /// Seconds until the current window closes.
        retry_after_secs: u64,
    },
}

impl QuotaVerdict {
    pub fn is_over(&self) -> bool {
        matches!(self, QuotaVerdict::OverQuota { .. })
    }
}

/// Pure fixed-window math, shared by the Redis path and the local fallback
/// so the unit tests pin one semantics.
pub fn window_bucket(now_secs: u64, window_secs: u64) -> u64 {
    now_secs / window_secs.max(1)
}

/// Seconds until the window containing `now_secs` closes.
pub fn window_retry_after(now_secs: u64, window_secs: u64) -> u64 {
    let window_secs = window_secs.max(1);
    (window_secs - now_secs % window_secs).max(1)
}

/// Fixed-window counter shared by the HTTP middleware and the notification
/// gateway. Authoritative in Redis (shared by every API instance); falls
/// back to an in-process window when Redis is absent or errors.
pub struct AbuseCounter {
    conn: Option<redis::aio::ConnectionManager>,
    script: redis::Script,
    local: Mutex<HashMap<String, LocalWindow>>,
}

#[derive(Debug, Clone, Copy)]
struct LocalWindow {
    bucket: u64,
    count: u64,
}

impl AbuseCounter {
    pub fn new(conn: Option<redis::aio::ConnectionManager>) -> Self {
        Self {
            conn,
            script: redis::Script::new(LUA_FIXED_WINDOW),
            local: Mutex::new(HashMap::new()),
        }
    }

    /// Count one event against `quota`. The count increments even when the
    /// verdict is `OverQuota` — a request that gets blocked still occupies a
    /// slot in its window, so a flood cannot outlast its own block by
    /// racing the window edge.
    pub async fn count(
        &self,
        surface: &str,
        key_kind: &str,
        key: &str,
        quota: crate::config::WindowQuota,
    ) -> QuotaVerdict {
        let now = unix_now();
        let bucket = window_bucket(now, quota.window_secs);

        let observed = match &self.conn {
            Some(conn) => {
                match self
                    .count_in_redis(conn, surface, key_kind, key, bucket, quota)
                    .await
                {
                    Some(n) => Some(n),
                    // Redis is configured but answered with an error. Falling
                    // back to the in-process window keeps the shadow series
                    // continuous and keeps an enforced quota from becoming a
                    // way to refuse the beacon by attacking Redis.
                    None => self.count_locally(surface, key_kind, key, bucket),
                }
            }
            None => self.count_locally(surface, key_kind, key, bucket),
        };

        match observed {
            Some(n) if n <= quota.limit => QuotaVerdict::WithinQuota,
            Some(_) => QuotaVerdict::OverQuota {
                retry_after_secs: window_retry_after(now, quota.window_secs),
            },
            // Unreachable: both counting paths return a count.
            None => QuotaVerdict::WithinQuota,
        }
    }

    async fn count_in_redis(
        &self,
        conn: &redis::aio::ConnectionManager,
        surface: &str,
        key_kind: &str,
        key: &str,
        bucket: u64,
        quota: crate::config::WindowQuota,
    ) -> Option<u64> {
        let redis_key = format!("sanvi:abuse:{surface}:{key_kind}:{key}:{bucket}");
        let mut conn = conn.clone();
        let result: redis::RedisResult<i64> = self
            .script
            .key(redis_key)
            .arg(quota.window_secs)
            .invoke_async(&mut conn)
            .await;
        match result {
            Ok(n) => Some(n.max(0) as u64),
            Err(err) => {
                tracing::warn!(error = %err, surface, "abuse quota count failed; using in-process window");
                None
            }
        }
    }

    fn count_locally(&self, surface: &str, key_kind: &str, key: &str, bucket: u64) -> Option<u64> {
        let map_key = format!("{surface}:{key_kind}:{key}");
        let mut local = self.local.lock().expect("abuse counter mutex poisoned");
        if local.len() >= MAX_LOCAL_KEYS && !local.contains_key(&map_key) {
            // Drop windows that have already closed; the keys that remain
            // are the ones the current flood is actually using.
            local.retain(|_, window| window.bucket == bucket);
        }
        let window = local
            .entry(map_key)
            .or_insert(LocalWindow { bucket, count: 0 });
        if window.bucket != bucket {
            *window = LocalWindow { bucket, count: 0 };
        }
        window.count += 1;
        Some(window.count)
    }

    /// In-process count for the current window — test and diagnostic
    /// introspection. The Redis series is authoritative when configured.
    pub fn local_count(&self, surface: &str, key_kind: &str, key: &str) -> u64 {
        let map_key = format!("{surface}:{key_kind}:{key}");
        self.local
            .lock()
            .expect("abuse counter mutex poisoned")
            .get(&map_key)
            .map_or(0, |window| window.count)
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Surface classification. The paths are code constants on purpose: which
// route belongs to which abuse surface is a security decision, and making
// it configuration invites silently pointing a quota at the wrong path.
// Thresholds stay configurable; membership does not.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbuseSurface {
    /// The public conversion beacon: unauthenticated, cross-origin, billed
    /// against ad-platform quotas downstream.
    Tracking,
    /// Invitation create and resend — each one sends mail with a
    /// credential-bearing URL.
    Invitation,
    /// Domain order placement: a registrar charge.
    DomainPurchase,
    /// The internal Kratos registration webhook: the backend's only
    /// signup surface (sign-up itself is a Kratos flow; the webhook is
    /// where a tenant record appears).
    Signup,
}

impl AbuseSurface {
    pub fn name(self) -> &'static str {
        match self {
            AbuseSurface::Tracking => "tracking",
            AbuseSurface::Invitation => "invitation",
            AbuseSurface::DomainPurchase => "domain_purchase",
            AbuseSurface::Signup => "signup",
        }
    }
}

/// `POST /api/v1/tenant/invitations/accept` shares the invitation path
/// prefix but is a join action, not a send: counting it toward the
/// invitation quota would let an accept flood shadow-flag a tenant that
/// never sent anything.
const INVITATION_ACCEPT_PATH: &str = "/api/v1/tenant/invitations/accept";

pub fn classify(method: &Method, path: &str) -> Option<AbuseSurface> {
    if method != Method::POST {
        return None;
    }
    match path {
        "/api/v1/public/track" => return Some(AbuseSurface::Tracking),
        "/api/v1/tenant/invitations" => return Some(AbuseSurface::Invitation),
        "/api/v1/tenant/domains/orders" => return Some(AbuseSurface::DomainPurchase),
        "/internal/kratos/registration" => return Some(AbuseSurface::Signup),
        _ => {}
    }
    if path.starts_with("/api/v1/tenant/invitations/") && path != INVITATION_ACCEPT_PATH {
        return Some(AbuseSurface::Invitation);
    }
    None
}

// ---------------------------------------------------------------------------
// Middleware.
// ---------------------------------------------------------------------------

/// The abuse-quota gate. Must be wrapped *inside* [`crate::middleware`]'s
/// `SecurityHeaders` (i.e. registered after it): a blocked tracking beacon
/// must carry the same response headers as an accepted one, or the header
/// set becomes the oracle the endpoint's uniform `202` exists to deny. It
/// runs after tenant resolution, whose result lives in the request
/// extensions by the time the inner layers execute.
#[derive(Clone)]
pub(crate) struct AbuseQuotaLayer {
    counter: Arc<AbuseCounter>,
    config: crate::config::AbuseControlsConfig,
}

impl AbuseQuotaLayer {
    pub fn new(counter: Arc<AbuseCounter>, config: crate::config::AbuseControlsConfig) -> Self {
        Self { counter, config }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for AbuseQuotaLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = AbuseQuotaService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(AbuseQuotaService {
            service: std::rc::Rc::new(service),
            counter: self.counter.clone(),
            config: self.config.clone(),
        }))
    }
}

pub(crate) struct AbuseQuotaService<S> {
    service: std::rc::Rc<S>,
    counter: Arc<AbuseCounter>,
    config: crate::config::AbuseControlsConfig,
}

/// The tracking beacon's uniform response, reproduced exactly (status,
/// body, content type) from the handler's `accepted()` so the two are
/// indistinguishable from outside.
fn tracking_accepted() -> HttpResponse {
    HttpResponse::Accepted().json(serde_json::json!({}))
}

impl<S> actix_web::dev::Service<ServiceRequest> for AbuseQuotaService<S>
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
        let Some(surface) = classify(req.method(), req.path()) else {
            let svc = std::rc::Rc::clone(&self.service);
            return Box::pin(async move { svc.call(req).await });
        };

        let surface_config = self.config.for_surface(surface);
        let mut dimensions: Vec<(&'static str, String, crate::config::WindowQuota)> = Vec::new();

        if let Some(quota) = surface_config.per_ip {
            let ip = req
                .connection_info()
                .realip_remote_addr()
                .map(str::to_string)
                .unwrap_or_else(|| "unknown".to_string());
            dimensions.push(("ip", ip, quota));
        }
        if let (Some(quota), true) = (surface_config.per_origin, surface == AbuseSurface::Tracking)
            && let Some(origin) = req
                .headers()
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok())
        {
            dimensions.push(("origin", origin.to_string(), quota));
        }
        if let Some(quota) = surface_config.per_tenant
            && let Some(context) = req.extensions().get::<crate::tenancy::TenantContext>()
        {
            dimensions.push(("tenant", context.tenant_id.to_string(), quota));
        }

        if dimensions.is_empty() {
            let svc = std::rc::Rc::clone(&self.service);
            return Box::pin(async move { svc.call(req).await });
        }

        let svc = std::rc::Rc::clone(&self.service);
        let counter = self.counter.clone();
        let enforce = surface_config.enforce;
        let request_id = req.extensions().get::<RequestId>().copied();
        let path = req.path().to_string();
        let instruments = crate::metrics::AbuseInstruments::process();
        Box::pin(async move {
            let mut denied_retry_after: Option<u64> = None;
            for (key_kind, key, quota) in &dimensions {
                let verdict = counter.count(surface.name(), key_kind, key, *quota).await;
                let outcome = match verdict {
                    QuotaVerdict::WithinQuota => "counted",
                    QuotaVerdict::OverQuota { .. } if enforce => "blocked",
                    QuotaVerdict::OverQuota { .. } => "would_block",
                };
                instruments.record(surface.name(), key_kind, outcome);
                // Shadow mode records the would-block and stops there; the
                // response is never touched.
                if enforce && let QuotaVerdict::OverQuota { retry_after_secs } = verdict {
                    denied_retry_after = Some(match denied_retry_after {
                        Some(existing) => existing.min(retry_after_secs),
                        None => retry_after_secs,
                    });
                }
            }

            let Some(retry_after_secs) = denied_retry_after else {
                return svc.call(req).await;
            };

            // The beacon's abuse control stays silent — a 429 would tell a
            // prober the flood was noticed, which is the one thing this
            // endpoint must never do. Metrics carry the block.
            if surface == AbuseSurface::Tracking {
                tracing::debug!(
                    surface = surface.name(),
                    path = %path,
                    "abuse quota exceeded; beacon dropped"
                );
                let response = tracking_accepted();
                return Ok(req.into_response(response.map_into_boxed_body()));
            }

            // No surface ships with enforcement on today; this is the path
            // a future config flip lands on, kept identical to the per-IP
            // limiter's 429 so clients see one rate-limit shape.
            tracing::warn!(
                surface = surface.name(),
                path = %path,
                request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                "request rejected: abuse quota exceeded"
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
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AbuseControlsConfig, SurfaceAbuseConfig, WindowQuota};
    // Imported by item, not as `test`: a bare `use actix_web::test` would
    // shadow the builtin `#[test]` attribute for the sync tests below.
    use actix_web::test::{TestRequest, call_service, init_service};
    use actix_web::{App, web};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn quota(limit: u64, window_secs: u64) -> WindowQuota {
        WindowQuota { limit, window_secs }
    }

    fn surface_config(enforce: bool, per_ip: WindowQuota) -> SurfaceAbuseConfig {
        SurfaceAbuseConfig {
            enforce,
            per_ip: Some(per_ip),
            per_origin: None,
            per_tenant: None,
        }
    }

    /// A handler that counts its own invocations, so tests can prove what
    /// reached the application regardless of response body. Every abuse
    /// route is a POST, so one handler shape serves all of them.
    async fn counting_handler(hits: &AtomicUsize) -> HttpResponse {
        hits.fetch_add(1, Ordering::SeqCst);
        tracking_accepted()
    }

    async fn post(
        app: &impl actix_web::dev::Service<
            actix_http::Request,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        >,
        path: &str,
        origin: Option<&str>,
    ) -> (u16, Vec<u8>, bool) {
        let mut request = TestRequest::post().uri(path);
        if let Some(origin) = origin {
            request = request.insert_header((header::ORIGIN, origin));
        }
        let res = call_service(app, request.to_request()).await;
        let status = res.status().as_u16();
        let retry_after = res.headers().contains_key(header::RETRY_AFTER);
        let body = actix_web::body::to_bytes(res.into_body())
            .await
            .unwrap()
            .to_vec();
        (status, body, retry_after)
    }

    #[test]
    fn window_math_pins_the_fixed_window_semantics() {
        assert_eq!(window_bucket(0, 60), 0);
        assert_eq!(window_bucket(59, 60), 0);
        assert_eq!(window_bucket(60, 60), 1);
        // Window edge honesty: a burst at the seam admits up to 2x limit.
        assert_eq!(window_retry_after(0, 60), 60);
        assert_eq!(window_retry_after(59, 60), 1);
        assert_eq!(window_retry_after(60, 60), 60);
    }

    #[actix_web::test]
    async fn local_counter_admits_to_the_limit_then_reports_over() {
        let counter = AbuseCounter::new(None);
        let q = quota(2, 60);
        assert_eq!(
            counter.count("s", "ip", "a", q).await,
            QuotaVerdict::WithinQuota
        );
        assert_eq!(
            counter.count("s", "ip", "a", q).await,
            QuotaVerdict::WithinQuota
        );
        assert!(counter.count("s", "ip", "a", q).await.is_over());
        // Keys, kinds and surfaces never share a window.
        assert_eq!(
            counter.count("s", "ip", "b", q).await,
            QuotaVerdict::WithinQuota
        );
        assert_eq!(
            counter.count("s", "origin", "a", q).await,
            QuotaVerdict::WithinQuota
        );
        assert_eq!(
            counter.count("other", "ip", "a", q).await,
            QuotaVerdict::WithinQuota
        );
        assert_eq!(counter.local_count("s", "ip", "a"), 3);
    }

    /// The in-process window closes: after `window_secs`, the counter
    /// admits again.
    #[actix_web::test]
    async fn local_window_resets_after_the_window_passes() {
        let counter = AbuseCounter::new(None);
        let q = quota(1, 1);
        assert_eq!(
            counter.count("s", "ip", "k", q).await,
            QuotaVerdict::WithinQuota
        );
        assert!(counter.count("s", "ip", "k", q).await.is_over());
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert_eq!(
            counter.count("s", "ip", "k", q).await,
            QuotaVerdict::WithinQuota
        );
    }

    #[test]
    fn classification_matches_only_the_abuse_surfaces() {
        let post = Method::POST;
        assert_eq!(
            classify(&post, "/api/v1/public/track"),
            Some(AbuseSurface::Tracking)
        );
        assert_eq!(
            classify(&post, "/api/v1/tenant/invitations"),
            Some(AbuseSurface::Invitation)
        );
        assert_eq!(
            classify(&post, "/api/v1/tenant/invitations/123/resend"),
            Some(AbuseSurface::Invitation)
        );
        assert_eq!(
            classify(&post, "/internal/kratos/registration"),
            Some(AbuseSurface::Signup)
        );
        assert_eq!(
            classify(&post, "/api/v1/tenant/domains/orders"),
            Some(AbuseSurface::DomainPurchase)
        );

        // Accept is a join, not a send; revoke is a DELETE.
        assert_eq!(classify(&post, INVITATION_ACCEPT_PATH), None);
        assert_eq!(
            classify(&Method::DELETE, "/api/v1/tenant/invitations/123"),
            None
        );
        // Other orders subroutes (auto-renew, GET list) stay out.
        assert_eq!(
            classify(&post, "/api/v1/tenant/domains/orders/1/auto-renew"),
            None
        );
        assert_eq!(
            classify(&Method::GET, "/api/v1/tenant/domains/orders"),
            None
        );
    }

    /// The DoD flood test, at middleware level: enforcement holds the
    /// bucket — beyond the cap, nothing reaches the handler, and every
    /// response stays the uniform 202.
    #[actix_web::test]
    async fn enforced_tracking_holds_under_flood() {
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let app = init_service(
            App::new()
                .wrap(AbuseQuotaLayer::new(
                    Arc::new(AbuseCounter::new(None)),
                    AbuseControlsConfig {
                        tracking: surface_config(true, quota(5, 60)),
                        ..Default::default()
                    },
                ))
                .route(
                    "/api/v1/public/track",
                    web::post().to(move || {
                        let hits = handler_hits.clone();
                        async move { counting_handler(&hits).await }
                    }),
                ),
        )
        .await;

        let mut responses: Vec<(u16, Vec<u8>)> = Vec::new();
        for _ in 0..50 {
            let (status, body, retry_after) =
                post(&app, "/api/v1/public/track", Some("https://shop.example")).await;
            assert!(!retry_after, "the beacon never announces a block");
            responses.push((status, body));
        }

        assert_eq!(
            hits.load(Ordering::SeqCst),
            5,
            "only the quota's worth of beacons reached the handler"
        );
        assert!(responses.iter().all(|(status, _)| *status == 202));
        // Admitted and blocked responses are byte-identical.
        let first = &responses[0];
        assert!(
            responses.iter().all(|response| response == first),
            "responses diverged: {responses:?}"
        );
    }

    /// Shadow mode never touches the response: over-quota requests reach
    /// the handler unchanged.
    #[actix_web::test]
    async fn shadow_mode_passes_everything_through() {
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let app = init_service(
            App::new()
                .wrap(AbuseQuotaLayer::new(
                    Arc::new(AbuseCounter::new(None)),
                    AbuseControlsConfig {
                        tracking: surface_config(false, quota(3, 60)),
                        ..Default::default()
                    },
                ))
                .route(
                    "/api/v1/public/track",
                    web::post().to(move || {
                        let hits = handler_hits.clone();
                        async move { counting_handler(&hits).await }
                    }),
                ),
        )
        .await;

        for _ in 0..20 {
            let (status, _, _) =
                post(&app, "/api/v1/public/track", Some("https://shop.example")).await;
            assert_eq!(status, 202);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 20);
    }

    /// The future config-flip path for non-tracking surfaces: 429
    /// problem+json with Retry-After, the same shape the per-IP limiter
    /// emits.
    #[actix_web::test]
    async fn enforced_non_tracking_surface_answers_429_with_retry_after() {
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let app = init_service(
            App::new()
                .wrap(AbuseQuotaLayer::new(
                    Arc::new(AbuseCounter::new(None)),
                    AbuseControlsConfig {
                        invitation: surface_config(true, quota(2, 60)),
                        ..Default::default()
                    },
                ))
                .route(
                    "/api/v1/tenant/invitations",
                    web::post().to(move || {
                        let hits = handler_hits.clone();
                        async move { counting_handler(&hits).await }
                    }),
                ),
        )
        .await;

        let (status, _, _) = post(&app, "/api/v1/tenant/invitations", None).await;
        assert_eq!(status, 202);
        let (status, _, _) = post(&app, "/api/v1/tenant/invitations", None).await;
        assert_eq!(status, 202);
        let (status, body, retry_after) = post(&app, "/api/v1/tenant/invitations", None).await;
        assert_eq!(status, 429);
        assert!(retry_after, "429 must carry Retry-After");
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["status"], 429);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "the handler saw exactly the quota's worth"
        );
    }

    /// Per-key isolation: two callers under one quota each get their own
    /// window. The actix test service has one peer address, so isolation is
    /// driven through the Origin dimension on the beacon.
    #[actix_web::test]
    async fn keys_are_isolated_per_dimension_value() {
        let hits = Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let app = init_service(
            App::new()
                .wrap(AbuseQuotaLayer::new(
                    Arc::new(AbuseCounter::new(None)),
                    AbuseControlsConfig {
                        tracking: SurfaceAbuseConfig {
                            enforce: true,
                            per_ip: None,
                            per_origin: Some(quota(1, 60)),
                            per_tenant: None,
                        },
                        ..Default::default()
                    },
                ))
                .route(
                    "/api/v1/public/track",
                    web::post().to(move || {
                        let hits = handler_hits.clone();
                        async move { counting_handler(&hits).await }
                    }),
                ),
        )
        .await;

        for origin in ["https://a.example", "https://b.example"] {
            for _ in 0..3 {
                let (status, _, _) = post(&app, "/api/v1/public/track", Some(origin)).await;
                assert_eq!(status, 202);
            }
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "each origin got exactly one beacon through"
        );
    }
}
