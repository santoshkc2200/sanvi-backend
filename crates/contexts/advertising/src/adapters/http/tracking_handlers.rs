//! Named gateway operations. The service-owned contract snapshot supplies payload schemas.
use crate::adapters::remote::GatewayState;
use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{ApiError, CurrentSubject, CurrentTenant, RequiredAnyFeature, RequiredPermission};

#[utoipa::path(
    post,
    path = "/api/v1/public/track",
    tag = "advertising",
    operation_id = "public_track_conversion"
)]
pub async fn public_track_conversion(
    state: web::Data<GatewayState>,
    request: HttpRequest,
    body: Result<web::Bytes, actix_web::Error>,
) -> HttpResponse {
    crate::adapters::remote::public_proxy(state, request, body).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/tracking/settings",
    tag = "advertising",
    operation_id = "get_tracking_settings"
)]
pub async fn get_tracking_settings(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/tracking/site-key",
    tag = "advertising",
    operation_id = "get_tracking_site_key"
)]
pub async fn get_tracking_site_key(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    put,
    path = "/api/v1/tenant/ads/tracking/settings",
    tag = "advertising",
    operation_id = "put_tracking_settings"
)]
pub async fn put_tracking_settings(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/tracking/test-event",
    tag = "advertising",
    operation_id = "test_tracking_event"
)]
pub async fn test_tracking_event(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/conversions",
    tag = "advertising",
    operation_id = "list_conversions"
)]
pub async fn list_conversions(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/conversions/{id}/diagnostics",
    tag = "advertising",
    operation_id = "get_conversion_diagnostics"
)]
pub async fn get_conversion_diagnostics(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/conversions/{id}/retry",
    tag = "advertising",
    operation_id = "retry_conversion"
)]
pub async fn retry_conversion(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/audiences",
    tag = "advertising",
    operation_id = "list_audiences"
)]
pub async fn list_audiences(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/audiences",
    tag = "advertising",
    operation_id = "create_audience"
)]
pub async fn create_audience(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/audiences/{id}/refresh",
    tag = "advertising",
    operation_id = "refresh_audience"
)]
pub async fn refresh_audience(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

pub fn tracking_payload_config() -> web::PayloadConfig {
    web::PayloadConfig::default().limit(16 * 1024)
}
