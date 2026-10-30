//! Named gateway operations. The service-owned contract snapshot supplies payload schemas.
use crate::adapters::remote::GatewayState;
use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{ApiError, CurrentSubject, CurrentTenant, RequiredAnyFeature, RequiredPermission};

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/platforms",
    tag = "advertising",
    operation_id = "list_ad_platforms"
)]
pub async fn list_ad_platforms(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/review-status",
    tag = "advertising",
    operation_id = "poll_campaign_review_status"
)]
pub async fn poll_campaign_review_status(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/drift",
    tag = "advertising",
    operation_id = "resolve_campaign_drift"
)]
pub async fn resolve_campaign_drift(
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
    path = "/api/v1/tenant/ads/campaigns",
    tag = "advertising",
    operation_id = "create_campaign"
)]
pub async fn create_campaign(
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
    path = "/api/v1/tenant/ads/campaigns",
    tag = "advertising",
    operation_id = "list_campaigns"
)]
pub async fn list_campaigns(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}",
    tag = "advertising",
    operation_id = "get_campaign"
)]
pub async fn get_campaign(
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
    patch,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}",
    tag = "advertising",
    operation_id = "patch_campaign"
)]
pub async fn patch_campaign(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/validate",
    tag = "advertising",
    operation_id = "validate_campaign"
)]
pub async fn validate_campaign(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/publish",
    tag = "advertising",
    operation_id = "publish_campaign"
)]
pub async fn publish_campaign(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/pause",
    tag = "advertising",
    operation_id = "pause_campaign"
)]
pub async fn pause_campaign(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/resume",
    tag = "advertising",
    operation_id = "resume_campaign"
)]
pub async fn resume_campaign(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/changes",
    tag = "advertising",
    operation_id = "list_campaign_changes"
)]
pub async fn list_campaign_changes(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups",
    tag = "advertising",
    operation_id = "add_ad_group"
)]
pub async fn add_ad_group(
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
    patch,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}",
    tag = "advertising",
    operation_id = "update_ad_group"
)]
pub async fn update_ad_group(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads",
    tag = "advertising",
    operation_id = "add_ad"
)]
pub async fn add_ad(
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
    patch,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads/{ad_id}",
    tag = "advertising",
    operation_id = "update_ad"
)]
pub async fn update_ad(
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
    path = "/api/v1/tenant/ads/connections/{platform}/oauth/start",
    tag = "advertising",
    operation_id = "start_oauth"
)]
pub async fn start_oauth(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    _fresh_mfa: sanvi_http::FreshAal2Required,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/connections/{platform}/oauth/callback",
    tag = "advertising",
    operation_id = "oauth_callback"
)]
pub async fn oauth_callback(
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
    path = "/api/v1/tenant/ads/connections",
    tag = "advertising",
    operation_id = "create_connection"
)]
pub async fn create_connection(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    _fresh_mfa: sanvi_http::FreshAal2Required,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/connections",
    tag = "advertising",
    operation_id = "list_connections"
)]
pub async fn list_connections(
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
    delete,
    path = "/api/v1/tenant/ads/connections/{connection_id}",
    tag = "advertising",
    operation_id = "delete_connection"
)]
pub async fn delete_connection(
    state: web::Data<GatewayState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    permission: RequiredPermission,
    feature: RequiredAnyFeature,
    _fresh_mfa: sanvi_http::FreshAal2Required,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_tenant(state, tenant, subject, permission, feature, request, body).await
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/creatives",
    tag = "advertising",
    operation_id = "create_creative"
)]
pub async fn create_creative(
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
    path = "/api/v1/tenant/ads/creatives/{creative_id}",
    tag = "advertising",
    operation_id = "get_creative"
)]
pub async fn get_creative(
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
    path = "/api/v1/tenant/ads/creatives",
    tag = "advertising",
    operation_id = "list_creatives"
)]
pub async fn list_creatives(
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
    delete,
    path = "/api/v1/tenant/ads/creatives/{creative_id}",
    tag = "advertising",
    operation_id = "delete_creative"
)]
pub async fn delete_creative(
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
    path = "/api/v1/tenant/ads/creatives/{creative_id}/previews",
    tag = "advertising",
    operation_id = "get_creative_previews"
)]
pub async fn get_creative_previews(
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
    path = "/api/v1/tenant/ads/metrics",
    tag = "advertising",
    operation_id = "get_ads_metrics"
)]
pub async fn get_ads_metrics(
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
    path = "/api/v1/tenant/ads/metrics/summary",
    tag = "advertising",
    operation_id = "get_ads_metrics_summary"
)]
pub async fn get_ads_metrics_summary(
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
    path = "/api/v1/tenant/ads/metrics/export",
    tag = "advertising",
    operation_id = "get_ads_metrics_export"
)]
pub async fn get_ads_metrics_export(
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
    path = "/api/v1/tenant/ads/metrics/freshness",
    tag = "advertising",
    operation_id = "get_ads_metrics_freshness"
)]
pub async fn get_ads_metrics_freshness(
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
    path = "/api/v1/tenant/ads/budget-caps",
    tag = "advertising",
    operation_id = "get_budget_caps"
)]
pub async fn get_budget_caps(
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
    path = "/api/v1/tenant/ads/budget-caps",
    tag = "advertising",
    operation_id = "put_budget_cap"
)]
pub async fn put_budget_cap(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
    tag = "advertising",
    operation_id = "get_campaign_budget_cap"
)]
pub async fn get_campaign_budget_cap(
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
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
    tag = "advertising",
    operation_id = "put_campaign_budget_cap"
)]
pub async fn put_campaign_budget_cap(
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
    path = "/api/v1/tenant/ads/budget-alerts",
    tag = "advertising",
    operation_id = "get_budget_alerts"
)]
pub async fn get_budget_alerts(
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
    path = "/api/v1/tenant/ads/budget-alerts/{alert_id}/acknowledge",
    tag = "advertising",
    operation_id = "acknowledge_budget_alert"
)]
pub async fn acknowledge_budget_alert(
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
    path = "/api/v1/tenant/ads/spend-status",
    tag = "advertising",
    operation_id = "get_spend_status"
)]
pub async fn get_spend_status(
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
    path = "/api/v1/platform/ads/health",
    tag = "advertising",
    operation_id = "get_platform_ads_health"
)]
pub async fn get_platform_ads_health(
    state: web::Data<GatewayState>,
    subject: CurrentSubject,
    permission: RequiredPermission,
    request: HttpRequest,
    body: web::Bytes,
) -> Result<HttpResponse, ApiError> {
    super::proxy_platform(state, subject, permission, request, body).await
}

/// Stable RFC-7807 suffixes emitted by future advertising endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AdvertisingProblemType {
    #[serde(rename = "advertising/platform-unavailable")]
    PlatformUnavailable,
    #[serde(rename = "advertising/connection-not-active")]
    ConnectionNotActive,
    #[serde(rename = "advertising/capability-unsupported")]
    CapabilityUnsupported,
    #[serde(rename = "advertising/token-expired")]
    TokenExpired,
    #[serde(rename = "advertising/directive-suppressed")]
    DirectiveSuppressed,
    #[serde(rename = "advertising/budget-cap-reached")]
    BudgetCapReached,
    #[serde(rename = "advertising/campaign-invalid")]
    CampaignInvalid,
}
