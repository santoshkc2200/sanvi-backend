use std::{sync::Arc, time::Duration};

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, ProblemDetail, RequiredAnyFeature, RequiredPermission,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::AdvertisingState;
use crate::{
    CapabilityMatrix,
    application::{DriftResolution, ResolvedCampaignDrift},
    domain::{AdvertisingError, Campaign, ConnectionState},
};

const DRIFT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(10 * 60);

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

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ResolveCampaignDriftRequest {
    pub revision: i64,
    pub resolution: DriftResolution,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResolvedCampaignDriftView {
    pub campaign: Campaign,
    pub revision: i64,
    pub resolution: DriftResolution,
    pub replayed: bool,
}

fn idempotency_key(req: &HttpRequest) -> Result<Uuid, ApiError> {
    let raw = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest {
            detail: "Idempotency-Key header is required for campaign drift resolution".to_string(),
        })?;
    Uuid::parse_str(raw).map_err(|_| ApiError::BadRequest {
        detail: "Idempotency-Key must be a UUID".to_string(),
    })
}

fn api_error(error: AdvertisingError) -> ApiError {
    match error {
        AdvertisingError::InvalidInput(detail) => ApiError::BadRequest { detail },
        AdvertisingError::PlatformUnavailable(detail) => ApiError::ServiceUnavailable { detail },
        AdvertisingError::Storage(detail) => {
            tracing::error!(error = %detail, "advertising drift resolution storage failure");
            ApiError::internal("advertising drift resolution failed")
        }
        AdvertisingError::StaleRevision => ApiError::Conflict {
            detail: "campaign revision is stale; refresh and resolve the latest drift".to_string(),
        },
        AdvertisingError::CampaignNotFound => ApiError::NotFound {
            resource: "campaign".to_string(),
        },
    }
}

/// Explicitly resolve a platform-observed campaign drift. The idempotency
/// claim is durable and created before any provider write; the adapter also
/// receives the same key for provider-side deduplication.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/drift",
    tag = "advertising",
    operation_id = "resolve_campaign_drift",
    request_body = ResolveCampaignDriftRequest,
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry of this drift resolution returns its original result.")
    ),
    responses(
        (status = 200, description = "Drift resolution applied or replayed", body = ResolvedCampaignDriftView),
        (status = 400, description = "Missing or malformed idempotency key, or invalid resolution", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail),
        (status = 409, description = "Stale revision, idempotency conflict, or resolution in progress", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn resolve_campaign_drift(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<Uuid>,
    req: HttpRequest,
    body: web::Json<ResolveCampaignDriftRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let campaign_id = path.into_inner();
    let command = body.into_inner();
    let mutation_id = idempotency_key(&req)?;
    // Revision is part of scope. A stale UI cannot turn a prior resolution
    // into a successful resolution of a later drift observation.
    let scope = format!(
        "tenant.{}.advertising.campaign.{}.drift.{}",
        tenant.0.tenant_id, campaign_id, command.revision
    );
    let key = mutation_id.to_string();
    match state
        .idempotency
        .claim(&key, &scope, DRIFT_IDEMPOTENCY_TTL)
        .await
        .map_err(|error| ApiError::internal(format!("idempotency claim failed: {error}")))?
    {
        Some(hit) => {
            let Some(response) = hit.response else {
                return Err(ApiError::Conflict {
                    detail: "campaign drift resolution is already in progress".to_string(),
                });
            };
            let mut view: ResolvedCampaignDriftView =
                serde_json::from_value(response).map_err(|error| {
                    ApiError::internal(format!(
                        "stored drift resolution response is invalid: {error}"
                    ))
                })?;
            if view.resolution != command.resolution {
                return Err(ApiError::Conflict {
                    detail: "Idempotency-Key was already used for a different drift resolution"
                        .to_string(),
                });
            }
            view.replayed = true;
            return Ok(HttpResponse::Ok().json(view));
        }
        None => {}
    }

    let resolved = state
        .resolve_campaign_drift
        .execute(
            tenant.0.tenant_id,
            campaign_id,
            command.revision,
            mutation_id,
            subject.0.user_id,
            command.resolution,
        )
        .await;
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            if let Err(release_error) = state.idempotency.release(&key, &scope).await {
                tracing::error!(error = %release_error, "advertising drift idempotency release failed");
            }
            return Err(api_error(error));
        }
    };
    let view = drift_view(resolved, command.resolution);
    let response = serde_json::to_value(&view).map_err(|error| {
        ApiError::internal(format!("drift response serialization failed: {error}"))
    })?;
    if let Err(error) = state.idempotency.resolve(&key, &scope, 200, response).await {
        // Mutation and audit record have already committed. Do not tell the
        // caller it failed and invite a retry after a successful provider
        // write; expiry makes this claim recoverable for observability.
        tracing::error!(error = %error, "advertising drift idempotency response storage failed");
    }
    Ok(HttpResponse::Ok().json(view))
}

fn drift_view(
    resolved: ResolvedCampaignDrift,
    resolution: DriftResolution,
) -> ResolvedCampaignDriftView {
    ResolvedCampaignDriftView {
        campaign: resolved.campaign,
        revision: resolved.revision,
        resolution,
        replayed: resolved.replayed,
    }
}
