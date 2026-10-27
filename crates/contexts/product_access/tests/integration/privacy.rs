use std::sync::{Arc, Mutex};

use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};
use sanvi_kernel::{TenantId, UserId};
use sanvi_privacy::domain::{EraseMode, PersonalDataProvider, PrivacyError, SubjectRef};
use sanvi_product_access::adapters::ProductAccessPrivacyProvider;
use sanvi_product_access::config::{ProductAccessConfig, UpstreamConfig};

const SERVICE_KEY: &str = "test-service-key-0001";
const SEED: &str = "0707070707070707070707070707070707070707070707070707070707070707";

#[derive(Default, Clone)]
struct Recorded {
    auth_header: Option<String>,
    token_header: Option<String>,
}

#[actix_web::test]
async fn export_pages_until_next_after_is_null() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_data = calls.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    let server = HttpServer::new(move || {
        let calls_data = calls_data.clone();
        App::new().route(
            "/internal/v1/privacy/users/{user_id}/records",
            web::get().to(move |req: HttpRequest| {
                let calls_data = calls_data.clone();
                async move {
                    let auth = req
                        .headers()
                        .get("Authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let token = req
                        .headers()
                        .get("X-Identity-Token")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let query = req.query_string().to_string();
                    calls_data.lock().unwrap().push(Recorded {
                        auth_header: auth,
                        token_header: token,
                    });

                    if query.contains("after=2") {
                        HttpResponse::Ok().json(serde_json::json!({
                            "records": [
                                {
                                    "kind": "store",
                                    "id": "3",
                                    "tenant_id": "1002",
                                    "store_id": "2",
                                    "name": "Store 2",
                                    "created_at": "2026-10-27T00:00:00Z"
                                }
                            ],
                            "next_after": null
                        }))
                    } else {
                        HttpResponse::Ok().json(serde_json::json!({
                            "records": [
                                {
                                    "kind": "store",
                                    "id": "1",
                                    "tenant_id": "1001",
                                    "store_id": "1",
                                    "name": "Store 1",
                                    "created_at": "2026-10-27T00:00:00Z"
                                },
                                {
                                    "kind": "product",
                                    "id": "2",
                                    "tenant_id": "1001",
                                    "store_id": "1",
                                    "name": "Prod 1",
                                    "created_at": "2026-10-27T00:00:00Z"
                                }
                            ],
                            "next_after": "2"
                        }))
                    }
                }
            }),
        )
    })
    .workers(1)
    .listen(listener)
    .unwrap()
    .run();

    let handle = server.handle();
    actix_web::rt::spawn(server);

    let config = ProductAccessConfig {
        signing_key_id: "dev-ed25519-1".into(),
        signing_key_seed: SEED.into(),
        ecommerce: UpstreamConfig {
            base_url: format!("http://{address}"),
            service_key: SERVICE_KEY.into(),
        },
        connectors: UpstreamConfig::default(),
    };
    let provider = ProductAccessPrivacyProvider::from_config(&config).unwrap();
    let subject = SubjectRef::user(UserId::try_from(3001_i64).unwrap());
    let bundle = provider.collect(&subject).await.unwrap();

    assert_eq!(bundle.entries.len(), 3);
    assert_eq!(bundle.context, "product_access");

    let requests = calls.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].auth_header.as_deref(),
        Some(format!("Bearer {SERVICE_KEY}").as_str())
    );
    assert_eq!(
        requests[1].auth_header.as_deref(),
        Some(format!("Bearer {SERVICE_KEY}").as_str())
    );
    assert!(requests[0].token_header.is_some());
    assert!(requests[1].token_header.is_some());
    assert_ne!(requests[0].token_header, requests[1].token_header);

    handle.stop(true).await;
}

#[actix_web::test]
async fn erase_reports_the_anonymised_count() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let calls_data = calls.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    let server = HttpServer::new(move || {
        let calls_data = calls_data.clone();
        App::new().route(
            "/internal/v1/privacy/users/{user_id}/erasure",
            web::post().to(move |req: HttpRequest| {
                let calls_data = calls_data.clone();
                async move {
                    let auth = req
                        .headers()
                        .get("Authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let token = req
                        .headers()
                        .get("X-Identity-Token")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    calls_data.lock().unwrap().push(Recorded {
                        auth_header: auth,
                        token_header: token,
                    });
                    HttpResponse::Ok().json(serde_json::json!({
                        "anonymised": 6
                    }))
                }
            }),
        )
    })
    .workers(1)
    .listen(listener)
    .unwrap()
    .run();

    let handle = server.handle();
    actix_web::rt::spawn(server);

    let config = ProductAccessConfig {
        signing_key_id: "dev-ed25519-1".into(),
        signing_key_seed: SEED.into(),
        ecommerce: UpstreamConfig {
            base_url: format!("http://{address}"),
            service_key: SERVICE_KEY.into(),
        },
        connectors: UpstreamConfig::default(),
    };
    let provider = ProductAccessPrivacyProvider::from_config(&config).unwrap();
    let subject = SubjectRef::user(UserId::try_from(3001_i64).unwrap());
    let report = provider.erase(&subject, EraseMode::Full).await.unwrap();

    assert_eq!(report.context, "product_access");
    assert_eq!(report.anonymised, 6);
    assert_eq!(report.erased, 0);

    let requests = calls.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].auth_header.as_deref(),
        Some(format!("Bearer {SERVICE_KEY}").as_str())
    );
    assert!(requests[0].token_header.is_some());

    handle.stop(true).await;
}

#[actix_web::test]
async fn upstream_failure_is_an_upstream_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    let server = HttpServer::new(|| {
        App::new().route(
            "/internal/v1/privacy/users/{user_id}/records",
            web::get()
                .to(|| async { HttpResponse::ServiceUnavailable().body("service unavailable") }),
        )
    })
    .workers(1)
    .listen(listener)
    .unwrap()
    .run();

    let handle = server.handle();
    actix_web::rt::spawn(server);

    let config = ProductAccessConfig {
        signing_key_id: "dev-ed25519-1".into(),
        signing_key_seed: SEED.into(),
        ecommerce: UpstreamConfig {
            base_url: format!("http://{address}"),
            service_key: SERVICE_KEY.into(),
        },
        connectors: UpstreamConfig::default(),
    };
    let provider = ProductAccessPrivacyProvider::from_config(&config).unwrap();
    let subject = SubjectRef::user(UserId::try_from(3001_i64).unwrap());
    let result = provider.collect(&subject).await;

    assert!(matches!(result, Err(PrivacyError::Upstream(_))));

    handle.stop(true).await;
}

#[actix_web::test]
async fn unconfigured_or_non_user_subject_is_empty() {
    let unconfigured =
        ProductAccessPrivacyProvider::from_config(&ProductAccessConfig::default()).unwrap();
    let user_subject = SubjectRef::user(UserId::try_from(3001_i64).unwrap());
    let bundle = unconfigured.collect(&user_subject).await.unwrap();
    assert!(bundle.entries.is_empty());
    let report = unconfigured
        .erase(&user_subject, EraseMode::Full)
        .await
        .unwrap();
    assert_eq!(report.anonymised, 0);

    let configured = ProductAccessPrivacyProvider::from_config(&ProductAccessConfig {
        signing_key_id: "dev-ed25519-1".into(),
        signing_key_seed: SEED.into(),
        ecommerce: UpstreamConfig {
            base_url: "http://127.0.0.1:59999".into(),
            service_key: SERVICE_KEY.into(),
        },
        connectors: UpstreamConfig::default(),
    })
    .unwrap();
    let end_user_subject = SubjectRef::end_user(TenantId::try_from(1001_i64).unwrap(), "cust-1");
    let bundle = configured.collect(&end_user_subject).await.unwrap();
    assert!(bundle.entries.is_empty());
    let report = configured
        .erase(&end_user_subject, EraseMode::Full)
        .await
        .unwrap();
    assert_eq!(report.anonymised, 0);
}
