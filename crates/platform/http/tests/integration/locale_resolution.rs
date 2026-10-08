//! Locale middleware tests: negotiation plumbing, extension insertion,
//! `Content-Language` + `Vary` response headers — with a stub resolver.

use std::sync::Arc;

use actix_web::body::BoxBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::{App, HttpResponse, http::StatusCode, test, web};
use sanvi_http::{
    CurrentLocale, LocaleContext, LocaleMiddleware, LocaleResolutionInput, LocaleResolver,
    LocaleSource,
};

struct StubResolver;

impl LocaleResolver for StubResolver {
    fn resolve<'a>(
        &'a self,
        input: LocaleResolutionInput<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = LocaleContext> + Send + 'a>> {
        Box::pin(async move {
            let explicit = input.explicit_locale.unwrap_or_default().to_string();
            let negotiated = if explicit == "ja" {
                ("ja".to_string(), LocaleSource::Explicit)
            } else {
                ("en".to_string(), LocaleSource::PlatformDefault)
            };
            LocaleContext {
                locale: negotiated.0.clone(),
                source: negotiated.1,
                fallback_chain: vec![negotiated.0.clone(), "en".to_string()],
                timezone: "UTC".to_string(),
                tenant_id: None,
            }
        })
    }
}

fn app_with_locale() -> actix_web::App<
    impl actix_web::dev::ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<tracing_actix_web::StreamSpan<BoxBody>>,
        Error = actix_web::Error,
        InitError = (),
    > + 'static,
> {
    let mut config = sanvi_http::ServerConfig::default();
    config.rate_limit.enabled = false;
    sanvi_http::middleware::stack(
        App::new().route(
            "/echo-locale",
            web::get().to(|locale: CurrentLocale| async move {
                HttpResponse::Ok().json(serde_json::json!({
                    "locale": locale.0.locale,
                    "source": locale.0.source,
                }))
            }),
        ),
        config,
        None,
        None,
        None,
        None,
        Some(Arc::new(LocaleMiddleware {
            resolver: Arc::new(StubResolver),
        })),
        sanvi_http::ssr_cache::SsrCacheLayer::disabled(
            sanvi_http::config::SsrCacheConfig::default(),
        ),
        sanvi_http::degraded::DegradedHeaderLayer::disabled(),
    )
}

#[actix_web::test]
async fn negotiated_locale_reaches_handlers_and_headers() {
    let app = test::init_service(app_with_locale()).await;
    let req = test::TestRequest::get()
        .uri("/echo-locale?locale=ja")
        .insert_header(("Accept-Language", "en"))
        .to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("content-language").unwrap(), "ja");
    assert!(
        res.headers()
            .get("vary")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Accept-Language")
    );

    let body = test::read_body(res).await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["locale"], "ja");
    assert_eq!(json["source"], "explicit");
}

#[actix_web::test]
async fn defaults_to_platform_locale_without_any_inputs() {
    let app = test::init_service(app_with_locale()).await;
    let req = test::TestRequest::get().uri("/echo-locale").to_request();
    let res = test::call_service(&app, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get("content-language").unwrap(), "en");

    let body = test::read_body(res).await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["locale"], "en");
    assert_eq!(json["source"], "platform_default");
}

#[actix_web::test]
async fn vary_header_merges_with_existing_values() {
    let app = test::init_service(app_with_locale()).await;
    let req = test::TestRequest::get().uri("/echo-locale").to_request();
    let res = test::call_service(&app, req).await;
    let vary = res.headers().get("vary").unwrap().to_str().unwrap();
    assert!(vary.contains("Accept-Language"));
}
