//! TASK-024 (slice 11.5e): abuse quotas against the assembled app and the
//! committed configuration.
//!
//! The middleware-level semantics (flood holds, shadow pass-through, window
//! reset, key isolation, the 429 flip path) are pinned by unit tests in
//! `crates/platform/http/src/abuse.rs`. What only this assembly can prove:
//! the quota gate sits inside the real stack and decides *before* the
//! router (advertising is not mounted here, so a beacon that passes the
//! quota falls through to a 404 while a blocked one is answered 202), the
//! committed `configs/base.toml` parses through the binary's own figment
//! chain into the shipping posture, and the Redis-backed counter enforces
//! one window across instances.

use std::sync::Arc;

// By item, not `use actix_web::test`: a bare import would shadow the
// builtin `#[test]` attribute for the sync tests below.
use actix_web::http::header;
use actix_web::test::{TestRequest, call_service, init_service};
use sanvi_api::AppConfig;
use sanvi_http::abuse::AbuseCounter;
use sanvi_http::{AbuseControlsConfig, QuotaVerdict, SurfaceAbuseConfig, WindowQuota};

use crate::http_contract::{test_app_with, test_config};

fn tiny_tracking(enforce: bool) -> AbuseControlsConfig {
    AbuseControlsConfig {
        tracking: SurfaceAbuseConfig {
            enforce,
            per_ip: Some(WindowQuota {
                limit: 3,
                window_secs: 60,
            }),
            per_origin: None,
            per_tenant: None,
        },
        ..AbuseControlsConfig::default()
    }
}

async fn flood_track<S, B>(app: &S) -> Vec<(u16, String, Vec<u8>)>
where
    S: actix_web::dev::Service<
            actix_http::Request,
            Response = actix_web::dev::ServiceResponse<B>,
            Error = actix_web::Error,
        >,
    B: actix_web::body::MessageBody,
{
    let mut observed = Vec::new();
    for _ in 0..8 {
        let req = TestRequest::post()
            .uri("/api/v1/public/track")
            .insert_header((header::ORIGIN, "https://shop.tenant.example"))
            .to_request();
        let res = call_service(app, req).await;
        let content_type = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let status = res.status().as_u16();
        let body = match actix_web::body::to_bytes(res.into_body()).await {
            Ok(bytes) => bytes.to_vec(),
            Err(_) => Vec::new(),
        };
        observed.push((status, content_type, body));
    }
    observed
}

/// Enforcement decides before the handler: the first `limit` beacons pass
/// the gate and fall through to the router (404 — the advertising context
/// is deliberately not mounted in this assembly); the rest are answered by
/// the gate itself with the beacon's uniform empty 202.
#[actix_web::test]
async fn enforced_tracking_quota_short_circuits_before_the_handler() {
    let mut config = test_config();
    config.server.abuse = tiny_tracking(true);
    let app = init_service(test_app_with(config)).await;

    let observed = flood_track(&app).await;
    let passed: Vec<_> = observed.iter().take(3).collect();
    assert!(
        passed.iter().all(|(status, _, _)| *status == 404),
        "within-quota beacons must reach the router unchanged: {passed:?}"
    );
    let blocked: Vec<_> = observed.iter().skip(3).collect();
    assert!(
        blocked.iter().all(|(status, _, _)| *status == 202),
        "over-quota beacons must be answered by the gate: {blocked:?}"
    );
    // The blocked response is the beacon's uniform answer, byte for byte.
    assert!(
        blocked.iter().all(
            |(_, content_type, body)| content_type.starts_with("application/json")
                && (body == b"{ }" || body == b"{}")
        ),
        "blocked responses are the empty JSON 202: {blocked:?}"
    );
}

/// Shadow mode is invisible: an over-quota flood still falls through to
/// the router every time — counting happened, nothing changed.
#[actix_web::test]
async fn shadow_quota_never_changes_the_response() {
    let mut config = test_config();
    config.server.abuse = tiny_tracking(false);
    let app = init_service(test_app_with(config)).await;

    let observed = flood_track(&app).await;
    assert!(
        observed.iter().all(|(status, _, _)| *status == 404),
        "shadow mode must not answer a single request: {observed:?}"
    );
}

/// The committed base config parses through the binary's own figment chain
/// and carries the shipping posture: counting on every surface, enforcement
/// on tracking only, the notification send quota present.
#[test]
fn committed_base_config_carries_the_abuse_posture() {
    use figment::providers::Format as _;
    let config: AppConfig = figment::Figment::new()
        .merge(figment::providers::Toml::file("configs/base.toml"))
        .extract()
        .expect("base.toml parses into AppConfig");

    assert!(config.server.abuse.tracking.enforce);
    assert_eq!(
        config.server.abuse.tracking.per_ip,
        Some(WindowQuota {
            limit: 120,
            window_secs: 60
        })
    );
    assert_eq!(
        config.server.abuse.tracking.per_origin,
        Some(WindowQuota {
            limit: 600,
            window_secs: 60
        })
    );
    for surface in [
        &config.server.abuse.invitation,
        &config.server.abuse.domain_purchase,
        &config.server.abuse.signup,
    ] {
        assert!(!surface.enforce, "only tracking enforces");
        assert!(surface.per_ip.is_some(), "counting is live");
    }
    assert_eq!(config.notification.send_quota_limit, 1000);
    assert_eq!(config.notification.send_quota_window_secs, 3600);
    config.validate().expect("committed posture validates");
}

/// Startup validation names the offending key, the HSTS pattern: an
/// operator typo is a refusal to boot, not a silently dead quota.
#[test]
fn startup_validation_names_an_invalid_abuse_quota() {
    let mut config = AppConfig::default();
    config.server.abuse.tracking.per_ip = Some(WindowQuota {
        limit: 0,
        window_secs: 60,
    });
    let err = config.validate().unwrap_err().to_string();
    assert!(
        err.contains("server.abuse.tracking.per_ip.limit"),
        "error must name the key: {err}"
    );
}

/// The Redis path is the cross-instance contract: two counters over one
/// Redis share a window, keys are isolated, and the TTL set by
/// `EXPIRE … NX` is present from the first increment.
#[actix_web::test]
async fn redis_backed_counter_shares_one_window_across_instances() {
    let url = sanvi_testsupport::redis_url().await;
    let client = redis::Client::open(url.as_str()).expect("test redis url parses");
    let conn = redis::aio::ConnectionManager::new(client)
        .await
        .expect("test redis connects");

    let instance_a = Arc::new(AbuseCounter::new(Some(conn.clone())));
    let instance_b = Arc::new(AbuseCounter::new(Some(conn.clone())));
    // Unique per run: the shared test Redis outlives the suite, and the
    // window bucket for a 60s window would otherwise still be warm.
    let key = format!("test-{}", uuid::Uuid::now_v7());
    let quota = WindowQuota {
        limit: 2,
        window_secs: 60,
    };

    assert_eq!(
        instance_a.count("s", "ip", &key, quota).await,
        QuotaVerdict::WithinQuota
    );
    assert_eq!(
        instance_b.count("s", "ip", &key, quota).await,
        QuotaVerdict::WithinQuota,
        "the second instance must see the first's count"
    );
    assert!(instance_a.count("s", "ip", &key, quota).await.is_over());

    // A different key has its own window on both instances.
    let other = format!("test-other-{}", uuid::Uuid::now_v7());
    assert_eq!(
        instance_b.count("s", "ip", &other, quota).await,
        QuotaVerdict::WithinQuota
    );

    // The key the flood filled carries a TTL set on first increment.
    let mut ttl_conn = conn.clone();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let bucket = now / quota.window_secs;
    let redis_key = format!("sanvi:abuse:s:ip:{key}:{bucket}");
    let ttl: i64 = redis::cmd("TTL")
        .arg(&redis_key)
        .query_async(&mut ttl_conn)
        .await
        .expect("TTL query works");
    assert!(
        ttl > 0 && ttl <= 60,
        "window key must expire with its window, got {ttl}"
    );
}
