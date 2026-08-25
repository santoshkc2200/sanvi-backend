//! `/public/track` and the tenant-facing tracking settings, test-event and
//! conversions endpoints (TASK-014, slice 10.5).
//!
//! `/public/track` is deliberately uninformative: it always answers `202`
//! with an empty body — a bad origin, an invalid site key, an oversized
//! payload, a rate-limit breach and a perfectly good event all look
//! identical from the outside. Reflecting anything about *why* a request
//! didn't take effect would hand a prober exactly the oracle this
//! endpoint exists to deny.

use actix_web::{HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{ApiError, CurrentTenant, ProblemDetail, RequiredPermission};
use utoipa::ToSchema;
use uuid::Uuid;

use sanvi_privacy::domain::SubjectRef;

use super::handlers::api_error;
use crate::adapters::http::AdvertisingState;
use crate::application::tracking::capture::TrackConversionCommand;
use crate::domain::{ClickIds, ConversionEvent, PlatformKey, TrackingSettings};

#[derive(serde::Deserialize, ToSchema)]
pub struct TrackConversionRequest {
    pub event_name: String,
    pub event_id: String,
    pub order_ref: Option<String>,
    #[serde(default)]
    pub click_ids: ClickIds,
    pub value: Option<i64>,
    pub currency: Option<String>,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub device_id: Option<String>,
}

#[derive(serde::Serialize, ToSchema)]
pub struct TestTrackingEventResponse {
    /// Always `true` — a marker so this synthetic call is never mistaken
    /// for a real event by anything reading the response.
    pub test: bool,
    pub captured: ConversionEvent,
}

fn accepted() -> HttpResponse {
    HttpResponse::Accepted().json(serde_json::json!({}))
}

/// A payload this small and this permissive doesn't need a real bot
/// detector; it needs to not process the handful of clients that
/// obviously never render a page: missing UA, or one of the common
/// scripted-client signatures. Anything more capable already looks
/// exactly like a browser and there is no reliable heuristic for it here
/// — that's a job for a dedicated bot-management layer, out of scope for
/// this task.
fn looks_like_bot(user_agent: Option<&str>) -> bool {
    let Some(user_agent) = user_agent else {
        return true;
    };
    let lowered = user_agent.to_ascii_lowercase();
    lowered.is_empty()
        || [
            "bot",
            "spider",
            "crawl",
            "curl/",
            "python-requests",
            "headlesschrome",
            "wget/",
        ]
        .iter()
        .any(|needle| lowered.contains(needle))
}

fn resolve_platforms(settings: &TrackingSettings, event_name: &str) -> Vec<PlatformKey> {
    settings
        .mappings
        .get(event_name)
        .map(|platform_actions| {
            platform_actions
                .keys()
                .filter_map(|key| PlatformKey::parse(key).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn build_command(
    tenant_id: Uuid,
    body: &TrackConversionRequest,
    platforms: Vec<PlatformKey>,
) -> TrackConversionCommand {
    let subject = match (body.email.as_deref(), body.device_id.as_deref()) {
        (Some(email), _) => SubjectRef::end_user(tenant_id, email),
        (None, Some(device_id)) => SubjectRef::device(device_id),
        (None, None) => SubjectRef::device("anonymous"),
    };

    TrackConversionCommand {
        tenant_id,
        event_name: body.event_name.clone(),
        event_id: body.event_id.clone(),
        order_ref: body.order_ref.clone(),
        click_ids: body.click_ids.clone(),
        value: body.value,
        currency: body.currency.clone(),
        email: body.email.clone(),
        phone: body.phone.clone(),
        subject,
        platforms,
    }
}

/// `web::JsonConfig` for this route only: a small body cap appropriate for
/// a tracking beacon, and an error handler that answers `202` instead of
/// letting actix's default extraction failure leak a 4xx that would tell
/// a prober their oversized/malformed payload was noticed.
pub fn tracking_json_config() -> web::JsonConfig {
    web::JsonConfig::default()
        .limit(16 * 1024)
        .error_handler(|_err, _req| {
            actix_web::error::InternalError::from_response("", accepted()).into()
        })
}

#[utoipa::path(
    post,
    path = "/api/v1/public/track",
    tag = "advertising",
    operation_id = "public_track_conversion",
    request_body = TrackConversionRequest,
    responses(
        (status = 202, description = "Always returned, regardless of outcome — see the module doc for why")
    )
)]
pub async fn public_track_conversion(
    req: HttpRequest,
    body: web::Json<TrackConversionRequest>,
    state: web::Data<AdvertisingState>,
) -> impl Responder {
    if looks_like_bot(
        req.headers()
            .get("User-Agent")
            .and_then(|v| v.to_str().ok()),
    ) {
        return accepted();
    }

    let Some(origin) = req.headers().get("Origin").and_then(|v| v.to_str().ok()) else {
        return accepted();
    };

    let Some(ip) = req.peer_addr().map(|addr| addr.ip().to_string()) else {
        return accepted();
    };
    let Some(_ip_permit) = state.tracking_ip_limiter.try_acquire(&ip) else {
        return accepted();
    };
    let Some(_domain_permit) = state.tracking_domain_limiter.try_acquire(origin) else {
        return accepted();
    };

    let Some(site_key) = req
        .headers()
        .get("X-Site-Key")
        .and_then(|v| v.to_str().ok())
    else {
        return accepted();
    };
    let Ok(tenant_id) = crate::application::tracking::site_key::verify_site_key(
        &state.config.tracking_site_key_signing_secret,
        site_key,
        origin,
    ) else {
        return accepted();
    };

    let settings = match state.settings_mgr.get_settings(tenant_id).await {
        Ok(settings) => settings,
        Err(_) => return accepted(),
    };
    let platforms = resolve_platforms(&settings, &body.event_name);
    let command = build_command(tenant_id, &body, platforms);

    // Errors here are swallowed by design (see the module doc): the
    // response never reveals whether capture succeeded, and the storage
    // layer's own idempotent insert is what makes a retried beacon safe.
    let _ = state.capture.execute(command).await;

    accepted()
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/tracking/settings",
    tag = "advertising",
    operation_id = "get_tracking_settings",
    responses(
        (status = 200, description = "Event-name to platform-conversion-action mappings", body = TrackingSettings),
        (status = 403, description = "Missing advertising.connect permission", body = ProblemDetail)
    )
)]
pub async fn get_tracking_settings(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let settings = state
        .settings_mgr
        .get_settings(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(settings))
}

#[utoipa::path(
    put,
    path = "/api/v1/tenant/ads/tracking/settings",
    tag = "advertising",
    operation_id = "put_tracking_settings",
    request_body = TrackingSettings,
    responses(
        (status = 200, description = "Settings saved"),
        (status = 403, description = "Missing advertising.connect permission", body = ProblemDetail)
    )
)]
pub async fn put_tracking_settings(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    body: web::Json<TrackingSettings>,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let mut settings = body.into_inner();
    settings.tenant_id = tenant.0.tenant_id;
    state
        .settings_mgr
        .update_settings(settings)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(serde_json::json!({"status": "ok"})))
}

/// Runs a synthetic event through the exact same capture pipeline as
/// `/public/track` — including the directive gate — and returns what was
/// captured and why, without persisting anything. Flagged as a test on
/// the client side; nothing here ever reaches `advertising.conversion_events`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/tracking/test-event",
    tag = "advertising",
    operation_id = "test_tracking_event",
    request_body = TrackConversionRequest,
    responses(
        (status = 200, description = "What was captured, the resolver's decision, and why", body = TestTrackingEventResponse),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail)
    )
)]
pub async fn test_tracking_event(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    body: web::Json<TrackConversionRequest>,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let settings = state
        .settings_mgr
        .get_settings(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    let platforms = resolve_platforms(&settings, &body.event_name);
    let command = build_command(tenant.0.tenant_id, &body, platforms);

    let captured = state.capture.dry_run(command).await.map_err(api_error)?;

    Ok(HttpResponse::Ok().json(TestTrackingEventResponse {
        test: true,
        captured,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/conversions",
    tag = "advertising",
    operation_id = "list_conversions",
    responses(
        (status = 200, description = "Captured conversion events, newest first", body = [ConversionEvent]),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail)
    )
)]
pub async fn list_conversions(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let events = state
        .list_conversions
        .execute(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(events))
}
