use std::sync::Arc;

use actix_web::{HttpResponse, web};
use sanvi_http::{ApiError, CurrentTenant, ProblemDetail, RequiredAnyFeature, RequiredPermission};
use serde::Serialize;
use utoipa::ToSchema;

use super::AdvertisingState;
use crate::{CapabilityMatrix, domain::ConnectionState};

/// Stable envelope for the platform catalog. TASK-010 fills this only from
/// registered capability matrices; no platform name is hard-coded here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PlatformsView {
    pub platforms: Vec<PlatformView>,
}

/// Registry-derived catalog item. A platform without entitlement remains in
/// the list so clients can render its upgrade state rather than a dead end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PlatformView {
    #[schema(example = "google_ads")]
    pub key: String,
    #[schema(example = "Google Ads")]
    pub display_name: String,
    pub entitlement_key: String,
    pub available: bool,
    pub upgrade_required: bool,
    pub connection_state: ConnectionState,
    pub capability_matrix: CapabilityMatrix,
}

/// A stored amount is always integer minor units plus the originating ad
/// account's ISO-4217 currency. JPY ¥2,000 is represented as `2000` / `JPY`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct MoneyView {
    #[schema(example = 2000)]
    pub amount_minor: i64,
    #[schema(example = "JPY")]
    pub currency: String,
}

/// A presentation-only conversion. Converted money is never persisted; its
/// rate date is required so chart and report values stay explainable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RenderedMoneyView {
    pub amount_minor: i64,
    pub currency: String,
    #[schema(value_type = String, format = Date, example = "2026-08-24")]
    pub fx_rate_date: String,
}

/// One daily chart rollup. The grain is `(tenant, platform, campaign, date)`;
/// tenant is supplied by the authenticated route scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct MetricPoint {
    #[schema(example = "google_ads")]
    pub platform: String,
    #[schema(value_type = String, format = Uuid)]
    pub campaign_id: String,
    #[schema(value_type = String, format = Date, example = "2026-08-24")]
    pub date: String,
    pub impressions: i64,
    pub clicks: i64,
    pub spend: MoneyView,
    pub conversion_value: MoneyView,
    pub rendered_spend: Option<RenderedMoneyView>,
}

/// Stable RFC-7807 suffixes emitted by future advertising endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
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
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/platforms",
    tag = "advertising",
    operation_id = "list_ad_platforms",
    responses(
        (status = 200, description = "Registry-derived, versioned advertising capability catalog", body = PlatformsView),
        (status = 403, description = "Missing advertising.read permission or both advertising platform entitlements", body = ProblemDetail)
    )
)]
pub async fn list_ad_platforms(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, ApiError> {
    // Read config so the state is intentional, without exposing any value.
    let _api_version = &state.config.google_ads_api_version;
    let mut platforms = Vec::new();
    for descriptor in state.catalog.execute() {
        let available = feature_gate
            .check(tenant.0.tenant_id, &descriptor.entitlement_key)
            .await?;
        platforms.push(PlatformView {
            key: descriptor.key.as_str().to_string(),
            display_name: descriptor.display_name,
            entitlement_key: descriptor.entitlement_key,
            available,
            upgrade_required: !available,
            connection_state: ConnectionState::NotConnected,
            capability_matrix: descriptor.capability_matrix,
        });
    }
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "private, max-age=60"))
        .json(PlatformsView { platforms }))
}
