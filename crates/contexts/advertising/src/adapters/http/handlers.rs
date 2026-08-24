use std::{sync::Arc, time::Duration};

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, FreshAal2Required, PROBLEM_TYPE_BASE, ProblemDetail,
    RequiredAnyFeature, RequiredPermission,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::AdvertisingState;
use crate::{
    CapabilityMatrix,
    application::{
        AdFields, AdGroupFields, CampaignFields, DriftResolution, NewAdFields, NewAdGroupFields,
        NewCampaignFields, ResolvedCampaignDrift,
    },
    domain::{
        AdGroup, AdvertisingConnection, AdvertisingError, BidSettings, Budget, Campaign,
        CampaignChange, ConnectionHealth, ConnectionState, Creative, Objective, PlatformAccount,
        PlatformKey, Schedule, StoredCampaign, Targeting, ValidationViolation,
    },
};

const DRIFT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(10 * 60);
const CAMPAIGN_IDEMPOTENCY_TTL: Duration = Duration::from_secs(10 * 60);
const DEFAULT_CHANGES_LIMIT: i64 = 50;

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
    #[serde(rename = "advertising/campaign-invalid")]
    CampaignInvalid,
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
        AdvertisingError::ConnectionNotFound => ApiError::NotFound {
            resource: "advertising connection".to_string(),
        },
        AdvertisingError::ConnectionNotActive => ApiError::Conflict {
            detail: "advertising connection is not active".to_string(),
        },
        AdvertisingError::CampaignNotPublished => ApiError::Conflict {
            detail: "campaign has not been published yet".to_string(),
        },
        AdvertisingError::AdGroupNotFound => ApiError::NotFound {
            resource: "ad group".to_string(),
        },
        AdvertisingError::AdNotFound => ApiError::NotFound {
            resource: "ad".to_string(),
        },
        // Normally intercepted by `campaign_validation_problem` so the
        // response carries field-addressed violations; the bare mapping
        // keeps this match exhaustive.
        AdvertisingError::Validation(violations) => ApiError::BadRequest {
            detail: format!(
                "campaign validation failed with {} violation(s)",
                violations.len()
            ),
        },
        AdvertisingError::EncryptionUnconfigured => {
            tracing::error!("advertising token encryption is not configured");
            ApiError::ServiceUnavailable {
                detail: "advertising connections are temporarily unavailable".to_string(),
            }
        }
        AdvertisingError::EncryptionFailure(detail) => {
            tracing::error!(error = %detail, "advertising token encryption failure");
            ApiError::internal("advertising connection could not be completed")
        }
    }
}

async fn require_platform_entitlement(
    state: &AdvertisingState,
    tenant_id: Uuid,
    platform: &crate::domain::PlatformKey,
    feature_gate: &Arc<dyn sanvi_http::FeatureGate>,
) -> Result<(), ApiError> {
    let descriptor = state
        .catalog
        .execute()
        .into_iter()
        .find(|descriptor| &descriptor.key == platform)
        .ok_or_else(|| ApiError::ServiceUnavailable {
            detail: "advertising platform is unavailable".to_string(),
        })?;
    if feature_gate
        .check(tenant_id, &descriptor.entitlement_key)
        .await?
    {
        Ok(())
    } else {
        Err(ApiError::Forbidden {
            detail: format!("feature not enabled: {}", descriptor.entitlement_key),
        })
    }
}

async fn require_campaign_write_entitlement(
    state: &AdvertisingState,
    tenant_id: Uuid,
    campaign_id: Uuid,
    feature_gate: &Arc<dyn sanvi_http::FeatureGate>,
) -> Result<(), ApiError> {
    let stored = state
        .get_campaign
        .execute(tenant_id, campaign_id)
        .await
        .map_err(api_error)?;
    require_platform_entitlement(state, tenant_id, &stored.platform, feature_gate).await
}

/// Domain validation failure → actix error whose problem body carries the
/// violations as RFC 9457 extension members, mirroring
/// `payments_problem`'s `DisconnectBlocked` handling.
fn campaign_validation_problem(violations: Vec<ValidationViolation>) -> actix_web::Error {
    let api = ApiError::BadRequest {
        detail: format!(
            "campaign validation failed with {} violation(s)",
            violations.len()
        ),
    };
    let mut problem = api.problem();
    problem.r#type = format!("{PROBLEM_TYPE_BASE}/advertising/campaign-invalid");
    problem.extensions = Some(
        [(
            "violations".to_string(),
            serde_json::to_value(&violations).unwrap_or(serde_json::Value::Array(vec![])),
        )]
        .into_iter()
        .collect(),
    );
    actix_web::error::InternalError::from_response(
        api.clone(),
        HttpResponse::build(api.status()).json(problem),
    )
    .into()
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
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, ApiError> {
    let campaign_id = path.into_inner();
    require_campaign_write_entitlement(
        &state,
        tenant.0.tenant_id,
        campaign_id,
        feature_gate.get_ref(),
    )
    .await?;
    let command = body.into_inner();
    let mutation_id = idempotency_key(&req)?;
    // Revision is part of scope. A stale UI cannot turn a prior resolution
    // into a successful resolution of a later drift observation.
    let scope = format!(
        "tenant.{}.advertising.campaign.{}.drift.{}",
        tenant.0.tenant_id, campaign_id, command.revision
    );
    let key = mutation_id.to_string();
    if let Some(hit) = state
        .idempotency
        .claim(&key, &scope, DRIFT_IDEMPOTENCY_TTL)
        .await
        .map_err(|error| ApiError::internal(format!("idempotency claim failed: {error}")))?
    {
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

// ---------------------------------------------------------------------------
// Campaign tree: draft/publish lifecycle, patch, pause/resume, changes.
// ---------------------------------------------------------------------------

/// One campaign plus the envelope fields the domain model does not carry:
/// which connection it belongs to and the platform-assigned id once
/// published.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CampaignView {
    pub id: Uuid,
    pub connection_id: Uuid,
    #[schema(example = "google_ads")]
    pub platform: String,
    pub external_id: Option<String>,
    pub revision: i64,
    pub campaign: Campaign,
}

impl From<StoredCampaign> for CampaignView {
    fn from(stored: StoredCampaign) -> Self {
        Self {
            id: stored.id,
            connection_id: stored.connection_id,
            platform: stored.platform.as_str().to_string(),
            external_id: stored.external_id,
            revision: stored.revision,
            campaign: stored.intent,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CampaignsView {
    pub campaigns: Vec<CampaignView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ValidationResultView {
    /// Empty means valid. Same shape a failed create or patch would carry.
    pub violations: Vec<ValidationViolation>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CampaignChangesView {
    pub changes: Vec<CampaignChange>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateCampaignRequest {
    pub connection_id: Uuid,
    pub name: String,
    #[schema(example = "sales")]
    pub objective: String,
    pub budget: Budget,
    pub schedule: Option<Schedule>,
}

/// A merge patch: every field is optional and `None` leaves the stored
/// value unchanged. Used both by `PATCH` and by `validate`'s dry run.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct PatchCampaignRequest {
    pub name: Option<String>,
    #[schema(example = "sales")]
    pub objective: Option<String>,
    pub budget: Option<Budget>,
    pub schedule: Option<Schedule>,
    pub ad_groups: Option<Vec<AdGroup>>,
}

impl PatchCampaignRequest {
    fn into_fields(self) -> Result<CampaignFields, ApiError> {
        let objective = self
            .objective
            .map(|value| {
                Objective::parse(&value).map_err(|error| ApiError::BadRequest {
                    detail: error.to_string(),
                })
            })
            .transpose()?;
        Ok(CampaignFields {
            name: self.name,
            objective,
            budget: self.budget,
            schedule: self.schedule,
            ad_groups: self.ad_groups,
        })
    }
}

fn if_match_revision(req: &HttpRequest) -> Result<i64, ApiError> {
    let raw = req
        .headers()
        .get("If-Match")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest {
            detail: "If-Match header with the campaign revision is required".to_string(),
        })?;
    raw.trim().parse().map_err(|_| ApiError::BadRequest {
        detail: "If-Match must be the campaign's integer revision".to_string(),
    })
}

/// Claim an idempotency key, returning the replayed value when one is
/// already stored. `None` means the caller owns the work and must resolve
/// or release the claim itself.
async fn claim_idempotent<T: serde::de::DeserializeOwned>(
    state: &AdvertisingState,
    key: &str,
    scope: &str,
) -> Result<Option<T>, ApiError> {
    match state
        .idempotency
        .claim(key, scope, CAMPAIGN_IDEMPOTENCY_TTL)
        .await
        .map_err(|error| ApiError::internal(format!("idempotency claim failed: {error}")))?
    {
        Some(hit) => {
            let Some(response) = hit.response else {
                return Err(ApiError::Conflict {
                    detail: "request with this idempotency key is already in progress".to_string(),
                });
            };
            let value = serde_json::from_value(response).map_err(|error| {
                ApiError::internal(format!("stored idempotent response is invalid: {error}"))
            })?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

async fn resolve_idempotent<T: Serialize>(
    state: &AdvertisingState,
    key: &str,
    scope: &str,
    value: &T,
) {
    let response = match serde_json::to_value(value) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(error = %error, "advertising idempotent response serialization failed");
            return;
        }
    };
    if let Err(error) = state.idempotency.resolve(key, scope, 200, response).await {
        // The mutation and its audit record already committed; surfacing
        // this as a caller-visible failure would only invite a retry after
        // a successful provider write.
        tracing::error!(error = %error, "advertising idempotent response storage failed");
    }
}

async fn release_idempotent(state: &AdvertisingState, key: &str, scope: &str) {
    if let Err(error) = state.idempotency.release(key, scope).await {
        tracing::error!(error = %error, "advertising idempotency release failed");
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns",
    tag = "advertising",
    operation_id = "create_campaign",
    request_body = CreateCampaignRequest,
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original draft.")
    ),
    responses(
        (status = 201, description = "Draft campaign created. Never touches the platform.", body = CampaignView),
        (status = 400, description = "Missing/malformed Idempotency-Key or invalid campaign fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Connection not found", body = ProblemDetail),
        (status = 409, description = "Connection is not active", body = ProblemDetail)
    )
)]
pub async fn create_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    req: HttpRequest,
    body: web::Json<CreateCampaignRequest>,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let mutation_id = idempotency_key(&req)?;
    let command = body.into_inner();
    let platform = state
        .create_campaign
        .platform_for_connection(tenant_id, command.connection_id)
        .await
        .map_err(api_error)?;
    require_platform_entitlement(&state, tenant_id, &platform, feature_gate.get_ref()).await?;
    let objective = Objective::parse(&command.objective).map_err(|error| ApiError::BadRequest {
        detail: error.to_string(),
    })?;
    let scope = format!("tenant.{tenant_id}.advertising.campaigns.create");
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Created().json(view));
    }

    let result = state
        .create_campaign
        .execute(
            tenant_id,
            command.connection_id,
            mutation_id,
            subject.0.user_id,
            NewCampaignFields {
                name: command.name,
                objective,
                budget: command.budget,
                schedule: command.schedule,
            },
        )
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(AdvertisingError::Validation(violations)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Created().json(view))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/campaigns",
    tag = "advertising",
    operation_id = "list_campaigns",
    responses(
        (status = 200, description = "Every campaign owned by the tenant, newest first", body = CampaignsView),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail)
    )
)]
pub async fn list_campaigns(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let campaigns = state
        .list_campaigns
        .execute(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(CampaignsView {
        campaigns: campaigns.into_iter().map(CampaignView::from).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}",
    tag = "advertising",
    operation_id = "get_campaign",
    params(("campaign_id" = Uuid, Path, description = "Campaign UUID")),
    responses(
        (status = 200, description = "The campaign's current intent and revision", body = CampaignView),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail)
    )
)]
pub async fn get_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<Uuid>,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let stored = state
        .get_campaign
        .execute(tenant.0.tenant_id, path.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(CampaignView::from(stored)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}",
    tag = "advertising",
    operation_id = "patch_campaign",
    request_body = PatchCampaignRequest,
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision; a stale value returns 409 with the current state."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Intent updated; if published, the edit was also pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn patch_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<Uuid>,
    req: HttpRequest,
    body: web::Json<PatchCampaignRequest>,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let campaign_id = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let expected_revision = if_match_revision(&req)?;
    let mutation_id = idempotency_key(&req)?;
    let fields = body.into_inner().into_fields()?;
    let scope =
        format!("tenant.{tenant_id}.advertising.campaign.{campaign_id}.patch.{expected_revision}");
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }

    let result = state
        .patch_campaign
        .execute(
            tenant_id,
            campaign_id,
            expected_revision,
            mutation_id,
            subject.0.user_id,
            fields,
        )
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(AdvertisingError::Validation(violations)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/validate",
    tag = "advertising",
    operation_id = "validate_campaign",
    request_body(content = PatchCampaignRequest, description = "Fields to validate; omitted fields keep the stored value. An empty body validates the campaign as stored.", content_type = "application/json"),
    params(("campaign_id" = Uuid, Path, description = "Campaign UUID")),
    responses(
        (status = 200, description = "Field-addressed violations; empty means valid. Never writes.", body = ValidationResultView),
        (status = 400, description = "Invalid objective in the candidate body", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail)
    )
)]
pub async fn validate_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<Uuid>,
    body: Option<web::Json<PatchCampaignRequest>>,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, ApiError> {
    let campaign_id = path.into_inner();
    require_campaign_write_entitlement(
        &state,
        tenant.0.tenant_id,
        campaign_id,
        feature_gate.get_ref(),
    )
    .await?;
    let fields = body
        .map(|body| body.into_inner().into_fields())
        .transpose()?
        .unwrap_or_default();
    let violations = state
        .validate_campaign
        .execute(tenant.0.tenant_id, campaign_id, fields)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(ValidationResultView { violations }))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/publish",
    tag = "advertising",
    operation_id = "publish_campaign",
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry after a timeout returns the one campaign that was created, never a second.")
    ),
    responses(
        (status = 200, description = "Live on the platform; external_id is now set. Replaying publish on an already-published campaign is a no-op success.", body = CampaignView),
        (status = 400, description = "Campaign fails platform validation (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail),
        (status = 409, description = "Idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn publish_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<Uuid>,
    req: HttpRequest,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let campaign_id = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let mutation_id = idempotency_key(&req)?;
    let scope = format!("tenant.{tenant_id}.advertising.campaign.{campaign_id}.publish");
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }

    let result = state
        .publish_campaign
        .execute(tenant_id, campaign_id, mutation_id, subject.0.user_id)
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(AdvertisingError::Validation(violations)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/pause",
    tag = "advertising",
    operation_id = "pause_campaign",
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("Idempotency-Key" = Uuid, Header, description = "Required.")
    ),
    responses(
        (status = 200, description = "Paused on the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed Idempotency-Key", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail),
        (status = 409, description = "Campaign is not yet published, or an idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn pause_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<Uuid>,
    req: HttpRequest,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let campaign_id = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let mutation_id = idempotency_key(&req)?;
    let scope = format!("tenant.{tenant_id}.advertising.campaign.{campaign_id}.pause");
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }
    let stored = match state
        .pause_campaign
        .execute(tenant_id, campaign_id, mutation_id, subject.0.user_id)
        .await
    {
        Ok(stored) => stored,
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/resume",
    tag = "advertising",
    operation_id = "resume_campaign",
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("Idempotency-Key" = Uuid, Header, description = "Required.")
    ),
    responses(
        (status = 200, description = "Resumed on the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed Idempotency-Key", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail),
        (status = 409, description = "Campaign is not yet published, or an idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn resume_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<Uuid>,
    req: HttpRequest,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let campaign_id = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let mutation_id = idempotency_key(&req)?;
    let scope = format!("tenant.{tenant_id}.advertising.campaign.{campaign_id}.resume");
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }
    let stored = match state
        .resume_campaign
        .execute(tenant_id, campaign_id, mutation_id, subject.0.user_id)
        .await
    {
        Ok(stored) => stored,
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/changes",
    tag = "advertising",
    operation_id = "list_campaign_changes",
    params(("campaign_id" = Uuid, Path, description = "Campaign UUID")),
    responses(
        (status = 200, description = "Newest-first attributed change log: actor, source (sanvi|platform), before/after state", body = CampaignChangesView),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail)
    )
)]
pub async fn list_campaign_changes(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<Uuid>,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let changes = state
        .list_campaign_changes
        .execute(tenant.0.tenant_id, path.into_inner(), DEFAULT_CHANGES_LIMIT)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(CampaignChangesView { changes }))
}

// ---------------------------------------------------------------------------
// Ad groups and ads: sub-resource shorthand over the same campaign tree
// `PATCH` already covers. Useful when a builder screen edits one ad group
// or ad without round-tripping the whole campaign body.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateAdGroupRequest {
    pub name: String,
    pub targeting: Targeting,
    pub bid: BidSettings,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct PatchAdGroupRequest {
    pub name: Option<String>,
    pub targeting: Option<Targeting>,
    pub bid: Option<BidSettings>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateAdRequest {
    pub creative: Creative,
    pub landing_url: String,
    pub tracking_template: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct PatchAdRequest {
    pub creative: Option<Creative>,
    pub landing_url: Option<String>,
    /// `None` leaves the stored value unchanged; there is no way to clear it
    /// back to unset in this slice (same limitation `PATCH /campaigns/{id}`
    /// has for `schedule`).
    pub tracking_template: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups",
    tag = "advertising",
    operation_id = "add_ad_group",
    request_body = CreateAdGroupRequest,
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Ad group appended; if published, pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn add_ad_group(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<Uuid>,
    req: HttpRequest,
    body: web::Json<CreateAdGroupRequest>,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let campaign_id = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let expected_revision = if_match_revision(&req)?;
    let mutation_id = idempotency_key(&req)?;
    let command = body.into_inner();
    let scope = format!(
        "tenant.{tenant_id}.advertising.campaign.{campaign_id}.ad_groups.add.{expected_revision}"
    );
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }

    let result = state
        .add_ad_group
        .execute(
            tenant_id,
            campaign_id,
            expected_revision,
            mutation_id,
            subject.0.user_id,
            NewAdGroupFields {
                name: command.name,
                targeting: command.targeting,
                bid: command.bid,
            },
        )
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(AdvertisingError::Validation(violations)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    patch,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}",
    tag = "advertising",
    operation_id = "update_ad_group",
    request_body = PatchAdGroupRequest,
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("ad_group_id" = String, Path, description = "Ad group id"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Ad group updated; if published, pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign or ad group not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn update_ad_group(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(Uuid, String)>,
    req: HttpRequest,
    body: web::Json<PatchAdGroupRequest>,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let (campaign_id, ad_group_id) = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let expected_revision = if_match_revision(&req)?;
    let mutation_id = idempotency_key(&req)?;
    let command = body.into_inner();
    let scope = format!(
        "tenant.{tenant_id}.advertising.campaign.{campaign_id}.ad_groups.{ad_group_id}.patch.{expected_revision}"
    );
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }

    let result = state
        .update_ad_group
        .execute(
            tenant_id,
            campaign_id,
            ad_group_id,
            expected_revision,
            mutation_id,
            subject.0.user_id,
            AdGroupFields {
                name: command.name,
                targeting: command.targeting,
                bid: command.bid,
            },
        )
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(AdvertisingError::Validation(violations)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads",
    tag = "advertising",
    operation_id = "add_ad",
    request_body = CreateAdRequest,
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("ad_group_id" = String, Path, description = "Ad group id"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Ad appended; if published, pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign or ad group not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn add_ad(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(Uuid, String)>,
    req: HttpRequest,
    body: web::Json<CreateAdRequest>,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let (campaign_id, ad_group_id) = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let expected_revision = if_match_revision(&req)?;
    let mutation_id = idempotency_key(&req)?;
    let command = body.into_inner();
    let scope = format!(
        "tenant.{tenant_id}.advertising.campaign.{campaign_id}.ad_groups.{ad_group_id}.ads.add.{expected_revision}"
    );
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }

    let result = state
        .add_ad
        .execute(
            tenant_id,
            campaign_id,
            ad_group_id,
            expected_revision,
            mutation_id,
            subject.0.user_id,
            NewAdFields {
                creative: command.creative,
                landing_url: command.landing_url,
                tracking_template: command.tracking_template,
            },
        )
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(AdvertisingError::Validation(violations)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

#[utoipa::path(
    patch,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/ad-groups/{ad_group_id}/ads/{ad_id}",
    tag = "advertising",
    operation_id = "update_ad",
    request_body = PatchAdRequest,
    params(
        ("campaign_id" = Uuid, Path, description = "Campaign UUID"),
        ("ad_group_id" = String, Path, description = "Ad group id"),
        ("ad_id" = String, Path, description = "Ad id"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Ad updated; if published, pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign, ad group or ad not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn update_ad(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(Uuid, String, String)>,
    req: HttpRequest,
    body: web::Json<PatchAdRequest>,
    _permission: RequiredPermission,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let (campaign_id, ad_group_id, ad_id) = path.into_inner();
    require_campaign_write_entitlement(&state, tenant_id, campaign_id, feature_gate.get_ref())
        .await?;
    let expected_revision = if_match_revision(&req)?;
    let mutation_id = idempotency_key(&req)?;
    let command = body.into_inner();
    let scope = format!(
        "tenant.{tenant_id}.advertising.campaign.{campaign_id}.ad_groups.{ad_group_id}.ads.{ad_id}.patch.{expected_revision}"
    );
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CampaignView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Ok().json(view));
    }

    let result = state
        .update_ad
        .execute(
            tenant_id,
            campaign_id,
            ad_group_id,
            ad_id,
            expected_revision,
            mutation_id,
            subject.0.user_id,
            AdFields {
                creative: command.creative,
                landing_url: command.landing_url,
                tracking_template: command.tracking_template,
            },
        )
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(AdvertisingError::Validation(violations)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(error) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
    };
    let view = CampaignView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Ok().json(view))
}

// --- Connections: OAuth start/callback, account selection, list, disconnect
// (TASK-011). --------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StartOAuthRequest {
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StartOAuthResponse {
    pub authorization_url: String,
    pub state: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/connections/{platform}/oauth/start",
    tag = "advertising",
    operation_id = "start_oauth",
    params(("platform" = String, Path, description = "google_ads or meta")),
    request_body = StartOAuthRequest,
    responses(
        (status = 200, description = "Authorization URL and opaque state; the frontend never constructs the URL and never sees a token", body = StartOAuthResponse),
        (status = 400, description = "Unknown platform or missing redirect_uri", body = ProblemDetail),
        (status = 403, description = "Missing advertising.connect permission, missing platform entitlement, or fresh MFA missing", body = ProblemDetail)
    )
)]
pub async fn start_oauth(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<String>,
    body: web::Json<StartOAuthRequest>,
    _permission: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let platform = PlatformKey::parse(&path.into_inner()).map_err(api_error)?;
    require_platform_entitlement(&state, tenant_id, &platform, feature_gate.get_ref()).await?;
    let (authorization_url, oauth_state) = state
        .start_authorization
        .execute(tenant_id, platform, body.into_inner().redirect_uri)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(StartOAuthResponse {
        authorization_url,
        state: oauth_state,
    }))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OAuthCallbackQuery {
    pub state: String,
    pub code: String,
    /// Must match the `redirect_uri` sent to `start` — the platform rejects
    /// a mismatch at exchange time.
    pub redirect_uri: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AccountView {
    pub external_id: String,
    pub display_name: String,
}

impl From<PlatformAccount> for AccountView {
    fn from(account: PlatformAccount) -> Self {
        Self {
            external_id: account.external_id,
            display_name: account.display_name,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PendingConnectionView {
    #[schema(value_type = String, format = Uuid)]
    pub id: Uuid,
    pub platform: String,
    pub status: String,
    pub accounts: Vec<AccountView>,
}

fn pending_connection_view(connection: AdvertisingConnection) -> PendingConnectionView {
    PendingConnectionView {
        id: connection.id(),
        platform: connection.platform().as_str().to_string(),
        status: connection.status().as_str().to_string(),
        accounts: connection
            .pending_accounts()
            .map(|accounts| accounts.iter().cloned().map(AccountView::from).collect())
            .unwrap_or_default(),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/connections/{platform}/oauth/callback",
    tag = "advertising",
    operation_id = "oauth_callback",
    params(
        ("platform" = String, Path, description = "google_ads or meta"),
        ("state" = String, Query, description = "Opaque state returned by start"),
        ("code" = String, Query, description = "Authorization code from the platform"),
        ("redirect_uri" = String, Query, description = "Must match the redirect_uri sent to start")
    ),
    responses(
        (status = 200, description = "Code redeemed; connection is pending account selection", body = PendingConnectionView),
        (status = 400, description = "Forged, expired, replayed, or cross-tenant state, or the code could not be exchanged", body = ProblemDetail),
        (status = 403, description = "Missing advertising.connect permission", body = ProblemDetail)
    )
)]
pub async fn oauth_callback(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<String>,
    query: web::Query<OAuthCallbackQuery>,
    _permission: RequiredPermission,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let platform = PlatformKey::parse(&path.into_inner()).map_err(api_error)?;
    let query = query.into_inner();
    let connection = state
        .handle_callback
        .execute(
            tenant_id,
            platform,
            &query.state,
            query.code,
            query.redirect_uri,
            Some(subject.0.user_id),
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(pending_connection_view(connection)))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateConnectionRequest {
    #[schema(value_type = String, format = Uuid)]
    pub connection_id: Uuid,
    pub external_account_id: String,
    #[schema(example = "JPY")]
    pub currency: String,
    #[schema(example = "Asia/Tokyo")]
    pub timezone: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectionHealthView {
    pub can_sync: bool,
    pub can_upload_conversions: bool,
    pub token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub scopes_missing: Vec<String>,
    pub last_error: Option<String>,
    pub last_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reconnect_required: bool,
}

impl From<ConnectionHealth> for ConnectionHealthView {
    fn from(health: ConnectionHealth) -> Self {
        Self {
            can_sync: health.can_sync,
            can_upload_conversions: health.can_upload_conversions,
            token_expires_at: health.token_expires_at,
            scopes_missing: health.scopes_missing,
            last_error: health.last_error,
            last_synced_at: health.last_synced_at,
            reconnect_required: health.reconnect_required,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectionView {
    #[schema(value_type = String, format = Uuid)]
    pub id: Uuid,
    pub platform: String,
    pub external_account_id: String,
    pub account_name: Option<String>,
    pub currency: String,
    pub timezone: String,
    pub status: String,
    pub health: ConnectionHealthView,
}

fn connection_view(connection: AdvertisingConnection, health: ConnectionHealth) -> ConnectionView {
    ConnectionView {
        id: connection.id(),
        platform: connection.platform().as_str().to_string(),
        external_account_id: connection.external_account_id().to_string(),
        account_name: connection.account_name().map(str::to_string),
        currency: connection.currency().to_string(),
        timezone: connection.timezone().to_string(),
        status: connection.status().as_str().to_string(),
        health: health.into(),
    }
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/connections",
    tag = "advertising",
    operation_id = "create_connection",
    request_body = CreateConnectionRequest,
    responses(
        (status = 200, description = "Connection finalized and active", body = ConnectionView),
        (status = 400, description = "Chosen account was not among the fetched accounts, or invalid currency/timezone", body = ProblemDetail),
        (status = 403, description = "Missing advertising.connect permission or fresh MFA missing", body = ProblemDetail),
        (status = 404, description = "Connection not found", body = ProblemDetail)
    )
)]
pub async fn create_connection(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    body: web::Json<CreateConnectionRequest>,
    _permission: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let command = body.into_inner();
    let connection = state
        .create_connection
        .execute(
            tenant_id,
            command.connection_id,
            &command.external_account_id,
            command.currency,
            command.timezone,
        )
        .await
        .map_err(api_error)?;
    let health = connection.health(&[], chrono::Utc::now());
    Ok(HttpResponse::Ok().json(connection_view(connection, health)))
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectionsView {
    pub connections: Vec<ConnectionView>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/connections",
    tag = "advertising",
    operation_id = "list_connections",
    responses(
        (status = 200, description = "Every connection with server-computed health", body = ConnectionsView),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail)
    )
)]
pub async fn list_connections(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let connections = state
        .list_connections
        .execute(tenant_id, &[])
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(ConnectionsView {
        connections: connections
            .into_iter()
            .map(|(connection, health)| connection_view(connection, health))
            .collect(),
    }))
}

#[utoipa::path(
    delete,
    path = "/api/v1/tenant/ads/connections/{connection_id}",
    tag = "advertising",
    operation_id = "delete_connection",
    params(("connection_id" = Uuid, Path, description = "Connection UUID")),
    responses(
        (status = 204, description = "Disconnected in Sanvi; the platform side is untouched"),
        (status = 403, description = "Missing advertising.connect permission or fresh MFA missing", body = ProblemDetail),
        (status = 404, description = "Connection not found", body = ProblemDetail)
    )
)]
pub async fn delete_connection(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<Uuid>,
    _permission: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let connection_id = path.into_inner();
    state
        .delete_connection
        .execute(tenant_id, connection_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::NoContent().finish())
}
