//! `/public/track` and the tenant-facing tracking settings, test-event and
//! conversions endpoints (TASK-014, slice 10.5).
//!
//! `/public/track` is deliberately uninformative: it always answers `202`
//! with an empty body — a bad origin, an invalid site key, an oversized
//! payload, a rate-limit breach and a perfectly good event all look
//! identical from the outside. Reflecting anything about *why* a request
//! didn't take effect would hand a prober exactly the oracle this
//! endpoint exists to deny.

use std::collections::BTreeMap;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, Responder, web};
use chrono::Utc;
use sanvi_http::{ApiError, CurrentTenant, ProblemDetail, RequiredPermission};
use sanvi_kernel::TenantId;
use utoipa::ToSchema;

use sanvi_privacy::domain::SubjectRef;

use super::handlers::api_error;
use crate::adapters::http::AdvertisingState;
use crate::application::tracking::capture::TrackConversionCommand;
use crate::domain::{
    Audience, AudienceId, ClickIds, ConversionDiagnostics, ConversionEvent, PlatformKey,
    TrackingSettings,
};

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

/// The platforms this event maps to, and whether that list is the
/// complete set of vendors the event will reach. An event name with no
/// mapping yet, and a mapped key this build cannot parse, both mean it
/// isn't: the gate must then ask for the third-party purposes anyway.
/// Dropping an unparseable key silently would defeat
/// `SnapshotVendorRoleLookup`'s protective `ThirdParty` default for
/// unknown platforms.
fn resolve_platforms(settings: &TrackingSettings, event_name: &str) -> (Vec<PlatformKey>, bool) {
    let Some(platform_actions) = settings.mappings.get(event_name) else {
        return (Vec::new(), true);
    };

    let mut platforms = Vec::new();
    let mut incomplete = platform_actions.is_empty();
    for key in platform_actions.keys() {
        match PlatformKey::parse(key) {
            Ok(platform) => platforms.push(platform),
            Err(_) => incomplete = true,
        }
    }
    (platforms, incomplete)
}

const MAX_LANDING_URL_BYTES: usize = 2_048;

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    serde_urlencoded::from_str::<BTreeMap<String, String>>(query)
        .ok()?
        .remove(key)
        .filter(|value| !value.is_empty())
}

/// Referers may contain order numbers, email addresses and other private
/// browser state in their query, fragment or user-info components. Keep the
/// navigable path for attribution, but never persist those components. Header
/// length is capped independently of the JSON request-body limit.
fn sanitize_landing_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > MAX_LANDING_URL_BYTES {
        return None;
    }

    let without_query_or_fragment = raw.split(['?', '#']).next()?.trim();
    let Some(scheme_end) = without_query_or_fragment.find("://") else {
        return Some(without_query_or_fragment.to_string());
    };
    let authority_start = scheme_end + 3;
    let authority_and_path = &without_query_or_fragment[authority_start..];
    let authority_end = authority_and_path
        .find('/')
        .unwrap_or(authority_and_path.len());
    let authority = &authority_and_path[..authority_end];
    let host = authority.rsplit('@').next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!(
        "{}{}{}",
        &without_query_or_fragment[..authority_start],
        host,
        &authority_and_path[authority_end..]
    ))
}

fn extract_server_click_ids(req: &HttpRequest, body_click_ids: &ClickIds) -> ClickIds {
    let referer = req
        .headers()
        .get("Referer")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty());

    let referer_query = referer.and_then(|url| {
        url.split_once('?')
            .map(|(_, q)| q.split_once('#').map(|(q_clean, _)| q_clean).unwrap_or(q))
    });

    let gclid = referer_query
        .and_then(|q| extract_query_param(q, "gclid"))
        .or_else(|| body_click_ids.gclid.clone());

    let gbraid = referer_query
        .and_then(|q| extract_query_param(q, "gbraid"))
        .or_else(|| body_click_ids.gbraid.clone());

    let wbraid = referer_query
        .and_then(|q| extract_query_param(q, "wbraid"))
        .or_else(|| body_click_ids.wbraid.clone());

    let fbc = req
        .cookie("_fbc")
        .map(|c| c.value().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| body_click_ids.fbc.clone());

    let fbp = req
        .cookie("_fbp")
        .map(|c| c.value().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| body_click_ids.fbp.clone());

    ClickIds {
        gclid,
        gbraid,
        wbraid,
        fbc,
        fbp,
        capture_time: body_click_ids.capture_time.or_else(|| Some(Utc::now())),
        landing_url: referer.and_then(sanitize_landing_url).or_else(|| {
            body_click_ids
                .landing_url
                .as_deref()
                .and_then(sanitize_landing_url)
        }),
    }
}

fn build_command(
    tenant_id: TenantId,
    body: &TrackConversionRequest,
    click_ids: ClickIds,
    (platforms, platforms_incomplete): (Vec<PlatformKey>, bool),
    gpc_device: Option<String>,
) -> TrackConversionCommand {
    // An email identifies a `Contact` (`email:{lowercased}`) — the key the
    // privacy ledger actually writes. `end_user` keys a merchant-supplied
    // external id, which no directive row here would ever match, so every
    // consented conversion would resolve to the most-protective fallback
    // and be suppressed. A device ref is only unique within the tenant
    // whose site emitted it, so it is scoped to that tenant rather than
    // shared platform-wide.
    //
    // When the GPC middleware observed this request it hands the handler
    // the platform-canonical device key ([`sanvi_http::GpcDevice`]) — the
    // key a `Sec-GPC` signal is recorded under. Consent resolution keys on
    // it in preference to the merchant's own `device_id`, so the signal
    // suppresses the event on the very first request (no cookie round-trip
    // exists yet for the merchant id and the canonical id to agree).
    let device_ref = gpc_device.or_else(|| body.device_id.clone());
    let subject = match (body.email.as_deref(), device_ref.as_deref()) {
        (Some(email), _) => SubjectRef::contact(email),
        (None, Some(device_id)) => SubjectRef::tenant_device(tenant_id, device_id),
        (None, None) => SubjectRef::tenant_device(tenant_id, "anonymous"),
    };

    TrackConversionCommand {
        tenant_id,
        event_name: body.event_name.clone(),
        event_id: body.event_id.clone(),
        order_ref: body.order_ref.clone(),
        click_ids,
        value: body.value,
        currency: body.currency.clone(),
        email: body.email.clone(),
        phone: body.phone.clone(),
        subject,
        platforms,
        platforms_incomplete,
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

    // `peer_addr` is the load balancer in every deployed environment, so
    // budgeting on it would collapse the whole platform into one per-IP
    // bucket while enforcing nothing against an actual abuser.
    let client_ip = req
        .connection_info()
        .realip_remote_addr()
        .map(|ip| ip.to_string());
    let Some(ip) = client_ip else {
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
    let Ok(tenant_id): Result<TenantId, _> =
        crate::application::tracking::site_key::verify_site_key(
            &state.config.tracking_site_key_signing_secret,
            site_key,
            origin,
        )
    else {
        return accepted();
    };

    let settings = match state.settings_mgr.get_settings(tenant_id).await {
        Ok(settings) => settings,
        Err(_) => return accepted(),
    };
    if let Some(runtime) = req.app_data::<web::Data<sanvi_http::quota_middleware::QuotaRuntime>>() {
        let accounting = async {
            runtime
                .service
                .configure_enforcement(tenant_id, runtime.enabled)
                .await?;
            // Public tracking authenticates the tenant here, after boundary
            // middleware. Count its API attempt in the same usage ledger.
            runtime
                .service
                .record(
                    tenant_id,
                    sanvi_http::quotas::QuotaDimension::ApiRequests,
                    1,
                    false,
                )
                .await?;
            if runtime.enabled {
                runtime
                    .service
                    .enforce(tenant_id, sanvi_http::quotas::QuotaDimension::ApiRequests)
                    .await?;
            }
            Ok::<_, sanvi_http::quotas::QuotaError>(())
        }
        .await;
        if let Err(error) = accounting {
            tracing::warn!(%error,"public tracking quota admission failed");
            if runtime.enabled {
                return accepted();
            }
        }
    }
    let platforms = resolve_platforms(&settings, &body.event_name);
    let click_ids = extract_server_click_ids(&req, &body.click_ids);
    let gpc_device = req
        .extensions()
        .get::<sanvi_http::GpcDevice>()
        .map(|device| device.0.clone());
    let command = build_command(tenant_id, &body, click_ids, platforms, gpc_device);

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

#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct TrackingSiteKeyView {
    pub site_key: String,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    pub origin: String,
}

#[derive(Debug, Clone, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TrackingSiteKeyQuery {
    /// Storefront origin to mint the site key for (must match the `Origin`
    /// header the pixel will later post with, e.g.
    /// `https://shop.example.com`). Parsed inside the handler (not via
    /// `web::Query`): extraction failures race the async authorization
    /// extractors and can answer 400 for a caller who was never
    /// authorized to learn anything (idor sweep finding).
    pub origin: String,
}

/// Per-tenant tracking site-key delivery (frontend TASK-014). The admin SPA
/// mints the public site key for a storefront origin and embeds it in page
/// source; `/public/track` later verifies `X-Site-Key` against the same
/// origin. Replaces the `PUBLIC_TRACKING_SITE_KEY` env fallback.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/tracking/site-key",
    tag = "advertising",
    operation_id = "get_tracking_site_key",
    params(TrackingSiteKeyQuery),
    responses(
        (status = 200, description = "Site key for the requested origin", body = TrackingSiteKeyView),
        (status = 400, description = "Missing or malformed origin", body = ProblemDetail),
        (status = 403, description = "Missing advertising.connect permission", body = ProblemDetail),
        (status = 503, description = "Tracking site-key signing secret is not configured", body = ProblemDetail)
    )
)]
pub async fn get_tracking_site_key(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    req: actix_web::HttpRequest,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    // Parsed after the tenant and permission extractors have run (see
    // the query struct's doc): the denial wins the race with a bad or
    // missing parameter, as the idor sweep demands.
    let query =
        web::Query::<std::collections::HashMap<String, String>>::from_query(req.query_string())
            .map_err(|_| ApiError::BadRequest {
                detail: "origin query parameter is required".to_string(),
            })?;
    let origin = query
        .get("origin")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    if origin.is_empty() || origin.len() > 2048 {
        return Err(ApiError::BadRequest {
            detail: "origin query parameter is required".to_string(),
        });
    }
    let site_key = crate::application::tracking::site_key::generate_site_key(
        &state.config.tracking_site_key_signing_secret,
        tenant.0.tenant_id,
        &origin,
    )
    .map_err(|e| match e {
        crate::domain::AdvertisingError::InvalidInput(detail)
            if detail.contains("signing secret") =>
        {
            ApiError::ServiceUnavailable { detail }
        }
        crate::domain::AdvertisingError::InvalidInput(detail) => ApiError::BadRequest { detail },
        other => api_error(other),
    })?;
    Ok(HttpResponse::Ok().json(TrackingSiteKeyView {
        site_key,
        tenant_id: tenant.0.tenant_id,
        origin,
    }))
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
    req: HttpRequest,
    body: web::Json<TrackConversionRequest>,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let settings = state
        .settings_mgr
        .get_settings(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    let platforms = resolve_platforms(&settings, &body.event_name);
    let gpc_device = req
        .extensions()
        .get::<sanvi_http::GpcDevice>()
        .map(|device| device.0.clone());
    let command = build_command(
        tenant.0.tenant_id,
        &body,
        body.click_ids.clone(),
        platforms,
        gpc_device,
    );

    let captured = state.capture.dry_run(command).await.map_err(api_error)?;

    Ok(HttpResponse::Ok().json(TestTrackingEventResponse {
        test: true,
        captured,
    }))
}

#[derive(Debug, Clone, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConversionListQuery {
    /// Page size, clamped to 1..=200.
    #[param(schema_with = conversion_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last conversion of the previous page. Absent
    /// for the first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn conversion_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 200).build()
}

/// Ordered by id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/conversions",
    tag = "advertising",
    operation_id = "list_conversions",
    params(ConversionListQuery),
    responses(
        (status = 200, description = "Captured conversion events, newest first", body = [ConversionEvent]),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_conversions(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    state: web::Data<AdvertisingState>,
    query: web::Query<ConversionListQuery>,
) -> Result<HttpResponse, ApiError> {
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let events = state
        .list_conversions
        .execute(tenant.0.tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(events))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/conversions/{id}/diagnostics",
    tag = "advertising",
    operation_id = "get_conversion_diagnostics",
    params(
        ("id" = String, Path, description = "Conversion event Snowflake ID")
    ),
    responses(
        (status = 200, description = "Conversion event diagnostics story", body = ConversionDiagnostics),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail),
        (status = 404, description = "Conversion not found", body = ProblemDetail)
    )
)]
pub async fn get_conversion_diagnostics(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    path: web::Path<crate::domain::model::ConversionId>,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let diagnostics = state
        .get_conversion_diagnostics
        .execute(tenant.0.tenant_id, path.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(diagnostics))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/conversions/{id}/retry",
    tag = "advertising",
    operation_id = "retry_conversion",
    params(
        ("id" = String, Path, description = "Conversion event Snowflake ID")
    ),
    responses(
        (status = 200, description = "Conversion event re-queued for upload", body = ConversionEvent),
        (status = 403, description = "Missing advertising.campaign.write permission", body = ProblemDetail),
        (status = 404, description = "Conversion not found", body = ProblemDetail),
        (status = 409, description = "Conversion retry refused", body = ProblemDetail)
    )
)]
pub async fn retry_conversion(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    path: web::Path<crate::domain::model::ConversionId>,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let event = state
        .retry_conversion
        .execute(tenant.0.tenant_id, path.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(event))
}

#[derive(Debug, Clone, serde::Deserialize, ToSchema)]
pub struct CreateAudienceRequest {
    pub name: String,
    pub platform: PlatformKey,
}

#[derive(Debug, Clone, serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AudienceListQuery {
    /// Page size, clamped to 1..=200.
    #[param(schema_with = conversion_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last audience of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    pub after: Option<String>,
}

/// Ordered by id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/audiences",
    tag = "advertising",
    operation_id = "list_audiences",
    params(AudienceListQuery),
    responses(
        (status = 200, description = "One page of the tenant's audiences", body = Vec<Audience>),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_audiences(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    state: web::Data<AdvertisingState>,
    query: web::Query<AudienceListQuery>,
) -> Result<HttpResponse, ApiError> {
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let audiences = state
        .list_audiences
        .execute(tenant.0.tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(audiences))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/audiences",
    tag = "advertising",
    operation_id = "create_audience",
    request_body = CreateAudienceRequest,
    responses(
        (status = 201, description = "Draft audience created", body = Audience),
        (status = 400, description = "Invalid audience parameters", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission", body = ProblemDetail)
    )
)]
pub async fn create_audience(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    state: web::Data<AdvertisingState>,
    body: web::Json<CreateAudienceRequest>,
) -> Result<HttpResponse, ApiError> {
    let audience = state
        .create_audience
        .execute(tenant.0.tenant_id, body.name.clone(), body.platform.clone())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(audience))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/audiences/{id}/refresh",
    tag = "advertising",
    operation_id = "refresh_audience",
    params(
        ("id" = String, Path, description = "The audience ID to refresh")
    ),
    responses(
        (status = 200, description = "Audience refreshed", body = Audience),
        (status = 403, description = "Missing advertising.campaign.write permission", body = ProblemDetail),
        (status = 404, description = "Audience not found", body = ProblemDetail)
    )
)]
pub async fn refresh_audience(
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    path: web::Path<AudienceId>,
    state: web::Data<AdvertisingState>,
) -> Result<HttpResponse, ApiError> {
    let audience = state
        .refresh_audience
        .execute(tenant.0.tenant_id, path.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(audience))
}

#[cfg(test)]
mod tests {
    use super::{MAX_LANDING_URL_BYTES, extract_query_param, sanitize_landing_url};

    #[test]
    fn query_values_are_form_url_decoded() {
        assert_eq!(
            extract_query_param("gclid=EAIaIQobCh%3D%3D&gbraid=gb+ref", "gclid"),
            Some("EAIaIQobCh==".to_string())
        );
        assert_eq!(
            extract_query_param("gclid=EAIaIQobCh%3D%3D&gbraid=gb+ref", "gbraid"),
            Some("gb ref".to_string())
        );
    }

    #[test]
    fn landing_url_drops_pii_bearing_referer_components_and_caps_length() {
        assert_eq!(
            sanitize_landing_url(
                "https://user:secret@store.example.test/thank-you?email=a.user@example.com#receipt"
            ),
            Some("https://store.example.test/thank-you".to_string())
        );
        assert_eq!(
            sanitize_landing_url(&"x".repeat(MAX_LANDING_URL_BYTES + 1)),
            None
        );
    }
}
