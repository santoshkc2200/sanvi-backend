//! The SSR response-cache negatives (TASK-022 11.3c) — the tests the task
//! file calls "the most valuable tests in the task", written against the
//! public middleware API before the middleware exists so the no-cache
//! behaviour is the one they are shown failing on.
//!
//! The invariants under test:
//! - an authenticated request (an `Authorization` on the request) is never
//!   served from or written to the cache;
//! - a response carrying `Set-Cookie` is never stored;
//! - two tenants cannot share an entry;
//! - 500 concurrent misses on one key produce exactly one render;
//! - a handler's `Cache-Control: no-store` wins over the cache;
//! - a hit replays the rendered status, headers and body byte-for-byte.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use actix_web::{App, HttpMessage, HttpResponse, http::header, test as actix_test, web};
#[allow(unused_imports)]
use sanvi_cache::SsrCacheKey as _;
use sanvi_cache::{DeviceClass, SsrCache};
use sanvi_http::locale::LocaleContext;
use sanvi_http::ssr_cache::{
    SsrCacheLayer, SsrKeyResolution, SsrKeyResolver, device_class_from_user_agent,
};
use sanvi_kernel::{SnowflakeId, TenantId};

fn tenant(raw: i64) -> TenantId {
    TenantId::from_snowflake(SnowflakeId::try_from(raw).unwrap())
}

/// A resolver that always answers with one tenant — the happy public host.
struct StaticResolver {
    tenant: TenantId,
}

impl SsrKeyResolver for StaticResolver {
    fn resolve<'a>(
        &'a self,
        _host: &'a str,
        _locale: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<SsrKeyResolution>> + Send + 'a>> {
        Box::pin(async move {
            Some(SsrKeyResolution {
                tenant_id: self.tenant,
                theme_revision: 3,
            })
        })
    }
}

/// A resolver whose answers rotate: the first `n` requests see the first
/// tenant, the rest see the second — the wrong-tenant probe.
struct RotatingResolver {
    tenants: Mutex<Vec<TenantId>>,
    revision: i32,
}

impl SsrKeyResolver for RotatingResolver {
    fn resolve<'a>(
        &'a self,
        _host: &'a str,
        _locale: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<SsrKeyResolution>> + Send + 'a>> {
        Box::pin(async move {
            let mut tenants = self.tenants.lock().unwrap();
            let tenant = if tenants.len() > 1 {
                tenants.rotate_left(1);
                tenants[0]
            } else {
                tenants[0]
            };
            drop(tenants);
            Some(SsrKeyResolution {
                tenant_id: tenant,
                theme_revision: self.revision,
            })
        })
    }
}

fn cache() -> SsrCache {
    SsrCache::new(None, None)
}

fn layer(cache: SsrCache, resolver: Arc<dyn SsrKeyResolver>) -> SsrCacheLayer {
    // The tests below exercise the flag-ON behaviour (the flag-off
    // position has its own test).
    SsrCacheLayer::new(
        sanvi_http::config::SsrCacheConfig {
            enabled: true,
            ttl_secs: 300,
        },
        cache,
        resolver,
    )
}

/// A storefront GET as the SSR frontend sends it: a Host header (the
/// request's tenant anchor). Requests without one bypass the cache by
/// design — there is nothing to resolve the tenant from.
fn crate_locale(lang: &str) -> LocaleContext {
    LocaleContext {
        locale: lang.to_string(),
        source: sanvi_http::locale::LocaleSource::Explicit,
        fallback_chain: vec![lang.to_string(), "en".to_string()],
        timezone: "UTC".to_string(),
        tenant_id: None,
    }
}

fn ssr_get(uri: &str) -> actix_test::TestRequest {
    actix_test::TestRequest::get()
        .uri(uri)
        .insert_header((header::HOST, "shop.tenant.example"))
}

type Renders = Arc<AtomicUsize>;

/// A storefront-ish handler: counts renders, answers a stable JSON body.
async fn counting_handler(renders: Renders) -> HttpResponse {
    let n = renders.fetch_add(1, Ordering::SeqCst);
    HttpResponse::Ok()
        .content_type("application/json")
        .body(format!(r#"{{"render":{n}}}"#))
}

/// The storefront-ish test app: counting handler routes over the cache
/// layer. Built inline per test so the concrete service type stays
/// nameable by inference.
macro_rules! storefront_app {
    ($cache:expr, $resolver:expr, $renders:expr $(,)?) => {
        actix_test::init_service({
            let renders = ($renders).clone();
            App::new().wrap(layer($cache, $resolver)).route(
                "/api/v1/public/theme",
                web::get().to(move || counting_handler(renders.clone())),
            )
        })
        .await
    };
}

/// The most valuable test in the task: a request carrying an `Authorization`
/// header is never served from the cache and never writes to it — not even
/// a request that arrives after an anonymous render of the same URL.
#[actix_web::test]
async fn authenticated_requests_are_never_served_from_or_written_to_the_cache() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let app = storefront_app!(
        cache(),
        Arc::new(StaticResolver { tenant: tenant(1) }),
        renders
    );

    // Anonymous first render fills the cache.
    let first = actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    assert_eq!(first.status(), 200);
    assert_eq!(renders.load(Ordering::SeqCst), 1);

    // The same URL with credentials: a fresh render, not a cached replay.
    let authed = actix_test::call_service(
        &app,
        actix_test::TestRequest::get()
            .uri("/api/v1/public/theme")
            .insert_header((header::AUTHORIZATION, "Bearer token"))
            .to_request(),
    )
    .await;
    assert_eq!(authed.status(), 200);
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "an authenticated request must render, never replay"
    );

    // And it must not have written either: the next anonymous request hits
    // the entry the *first* anonymous request stored.
    let third = actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    let body = actix_test::read_body(third).await;
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "an authenticated request must not store into the cache"
    );
    assert_eq!(body, actix_web::web::Bytes::from_static(br#"{"render":0}"#));
}

/// A response that sets a cookie is never stored: cookies make responses
/// per-recipient, and a shared cache that stores one serves one user's
/// cookie to another.
#[actix_web::test]
async fn a_response_carrying_set_cookie_is_never_stored() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let renders_for_handler = renders.clone();
    let app = actix_test::init_service(
        App::new()
            .wrap(layer(
                cache(),
                Arc::new(StaticResolver { tenant: tenant(1) }),
            ))
            .route(
                "/api/v1/public/theme",
                web::get().to(move || {
                    let renders = renders_for_handler.clone();
                    async move {
                        renders.fetch_add(1, Ordering::SeqCst);
                        HttpResponse::Ok()
                            .content_type("application/json")
                            .insert_header(("set-cookie", "session=abc; Path=/"))
                            .body(r#"{"render":1}"#)
                    }
                }),
            ),
    )
    .await;

    for _ in 0..2 {
        let res =
            actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
        assert_eq!(res.status(), 200);
    }
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "a Set-Cookie response must render per request, never be stored"
    );
}

#[actix_web::test]
async fn two_tenants_cannot_share_an_entry() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(RotatingResolver {
        tenants: Mutex::new(vec![tenant(1), tenant(2)]),
        revision: 3,
    });
    let app = storefront_app!(cache(), resolver, renders);

    let first = actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    let second = actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "a second tenant must render its own entry"
    );
    let first_body = actix_test::read_body(first).await;
    let second_body = actix_test::read_body(second).await;
    assert_ne!(first_body, second_body, "tenants must never share an entry");

    // The rotation is back at tenant 1: its stored entry is served.
    let third = actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    assert_eq!(actix_test::read_body(third).await, first_body);
    assert_eq!(renders.load(Ordering::SeqCst), 2);
}

/// The stampede contract: a cold cache under a 500-miss spike on one key
/// renders exactly once; every waiter receives the leader's bytes.
#[actix_web::test]
async fn five_hundred_concurrent_misses_render_exactly_once() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    // A plain Arc shared across the 500 concurrent join_all futures.
    #[allow(clippy::arc_with_non_send_sync)]
    let app = Arc::new(storefront_app!(
        cache(),
        Arc::new(StaticResolver { tenant: tenant(1) }),
        renders
    ));

    let futures = (0..500).map(|_| {
        let app = Arc::clone(&app);
        async move {
            let res = actix_test::call_service(
                app.as_ref(),
                ssr_get("/api/v1/public/theme").to_request(),
            )
            .await;
            assert_eq!(res.status(), 200);
            actix_test::read_body(res).await
        }
    });
    let bodies = futures_util::future::join_all(futures).await;
    for body in bodies.iter() {
        assert_eq!(body, &bodies[0], "every waiter receives the leader's bytes");
    }
    assert_eq!(
        renders.load(Ordering::SeqCst),
        1,
        "500 concurrent misses on one key must produce exactly one render"
    );
}

/// A handler's `Cache-Control: no-store` wins: the response is rendered per
/// request even though it is a 200 JSON GET on a cached prefix.
#[actix_web::test]
async fn a_no_store_response_is_never_stored() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let renders_for_handler = renders.clone();
    let app = actix_test::init_service(
        App::new()
            .wrap(layer(
                cache(),
                Arc::new(StaticResolver { tenant: tenant(1) }),
            ))
            .route(
                "/api/v1/public/theme",
                web::get().to(move || {
                    let renders = renders_for_handler.clone();
                    async move {
                        renders.fetch_add(1, Ordering::SeqCst);
                        HttpResponse::Ok()
                            .content_type("application/json")
                            .insert_header(("cache-control", "no-store"))
                            .body(r#"{"render":1}"#)
                    }
                }),
            ),
    )
    .await;

    for _ in 0..2 {
        actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    }
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "no-store must bypass the cache"
    );
}

/// A hit replays the rendered response: same bytes, same status, and the
/// headers the render produced (the ETag a conditional client relies on).
#[actix_web::test]
async fn a_hit_replays_status_headers_and_body() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let renders_for_handler = renders.clone();
    let app = actix_test::init_service(
        App::new()
            .wrap(layer(
                cache(),
                Arc::new(StaticResolver { tenant: tenant(1) }),
            ))
            .route(
                "/api/v1/public/theme",
                web::get().to(move || {
                    let renders = renders_for_handler.clone();
                    async move {
                        renders.fetch_add(1, Ordering::SeqCst);
                        HttpResponse::Ok()
                            .content_type("application/json")
                            .insert_header(("etag", "\"render-1\""))
                            .body(r#"{"render":1}"#)
                    }
                }),
            ),
    )
    .await;

    let first = actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    let second = actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    assert_eq!(renders.load(Ordering::SeqCst), 1);
    assert_eq!(first.status(), second.status());
    let first_head = first.response().headers().clone();
    let second_head = second.response().headers().clone();
    for name in [&header::CONTENT_TYPE, &header::ETAG] {
        assert_eq!(
            first_head.get(name),
            second_head.get(name),
            "{name} must replay"
        );
    }
    assert_eq!(
        actix_test::read_body(first).await,
        actix_test::read_body(second).await
    );
}

/// The flag-off position is the pre-cache behaviour: every request renders.
#[actix_web::test]
async fn the_flag_off_position_renders_every_request() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let config = sanvi_http::config::SsrCacheConfig::default(); // enabled = false
    let app = actix_test::init_service(
        App::new()
            .wrap(SsrCacheLayer::new(
                config,
                cache(),
                Arc::new(StaticResolver { tenant: tenant(1) }),
            ))
            .route(
                "/api/v1/public/theme",
                web::get().to({
                    let renders = renders.clone();
                    let renders = renders.clone();
                    move || counting_handler(renders.clone())
                }),
            ),
    )
    .await;

    for _ in 0..2 {
        actix_test::call_service(&app, ssr_get("/api/v1/public/theme").to_request()).await;
    }
    assert_eq!(renders.load(Ordering::SeqCst), 2, "flag off = no cache");
}

/// Requests off the storefront prefixes are never cached, whatever the flag
/// says: the cache exists for the public storefront reads only.
#[actix_web::test]
async fn requests_off_the_storefront_prefixes_are_never_cached() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let app = actix_test::init_service(
        App::new()
            .wrap(layer(
                cache(),
                Arc::new(StaticResolver { tenant: tenant(1) }),
            ))
            .route(
                "/api/v1/tenant/theme",
                web::get().to({
                    let renders = renders.clone();
                    let renders = renders.clone();
                    move || counting_handler(renders.clone())
                }),
            ),
    )
    .await;

    for _ in 0..2 {
        actix_test::call_service(&app, ssr_get("/api/v1/tenant/theme").to_request()).await;
    }
    assert_eq!(renders.load(Ordering::SeqCst), 2);
}

/// Device-class partition input: the storefront's mobile markup rides on a
/// coarse two-way classification of the User-Agent.
#[test]
fn device_class_classification_is_coarse_and_stable() {
    assert_eq!(
        device_class_from_user_agent(Some(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)"
        )),
        DeviceClass::Mobile
    );
    assert_eq!(
        device_class_from_user_agent(Some("Mozilla/5.0 (Linux; Android 14; Pixel 8; Mobile)")),
        DeviceClass::Mobile
    );
    assert_eq!(
        device_class_from_user_agent(Some("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")),
        DeviceClass::Desktop
    );
    // Android without "mobile" is a tablet: desktop markup.
    assert_eq!(
        device_class_from_user_agent(Some("Mozilla/5.0 (Linux; Android 14; SM-X910)")),
        DeviceClass::Desktop
    );
    assert_eq!(device_class_from_user_agent(None), DeviceClass::Desktop);
}

/// The negotiated locale participates in the key even when the URL is
/// identical — a wrong-locale replay is a wrong response. The production
/// locale middleware inserts `LocaleContext` before the cache layer runs;
/// the test inserts it directly.
#[actix_web::test]
async fn locale_partitions_the_served_entries() {
    let renders: Renders = Arc::new(AtomicUsize::new(0));
    let app = storefront_app!(
        cache(),
        Arc::new(StaticResolver { tenant: tenant(1) }),
        renders
    );

    let make = |lang: &str| {
        let req = ssr_get("/api/v1/public/theme").to_request();
        req.extensions_mut().insert(crate_locale(lang));
        req
    };
    let en = actix_test::call_service(&app, make("en")).await;
    let ja = actix_test::call_service(&app, make("ja")).await;
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "locale must partition the key"
    );
    let en2 = actix_test::call_service(&app, make("en")).await;
    assert_eq!(renders.load(Ordering::SeqCst), 2);
    assert_eq!(
        actix_test::read_body(en).await,
        actix_test::read_body(en2).await
    );
    drop(ja); // the ja request's own entry, not asserted further here
}
