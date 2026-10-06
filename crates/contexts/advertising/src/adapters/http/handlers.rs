use std::{sync::Arc, time::Duration};

use actix_web::{HttpRequest, HttpResponse, web};
use chrono::{DateTime, NaiveDate, Utc};
use sanvi_http::{
    ApiError, CurrentSubject, CurrentTenant, FreshAal2Required, PROBLEM_TYPE_BASE, ProblemDetail,
    RequiredAnyFeature, RequiredPermission,
};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::AdvertisingState;
use crate::domain::model::{
    BudgetAlert, BudgetAlertId, BudgetCap, BudgetPeriod, CampaignId, ConnectionId, CreativeId,
    Money,
};
use crate::{
    CapabilityMatrix,
    application::{
        AdFields, AdGroupFields, CampaignFields, DriftResolution, NewAdFields, NewAdGroupFields,
        NewCampaignFields, NewCreativeFields, PlacementPreview, PlatformAdsHealthReport,
        PublishError, PutBudgetCapCommand, PutBudgetCapResponse, ResolvedCampaignDrift,
        SpendStatusReport,
    },
    domain::{
        AdGroup, AdvertisingConnection, AdvertisingError, AssetMetadata, BidSettings, Budget,
        Campaign, CampaignChange, ConnectionFreshness, ConnectionHealth, ConnectionState, Creative,
        CreativeAssetSpec, CreativeText, MetricsGroupBy, MetricsRangeQuery, Objective,
        PlatformAccount, PlatformKey, Schedule, StoredCampaign, StoredCreative, Targeting,
        ValidationViolation,
    },
};

const DRIFT_IDEMPOTENCY_TTL: Duration = Duration::from_secs(10 * 60);
const CAMPAIGN_IDEMPOTENCY_TTL: Duration = Duration::from_secs(10 * 60);

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

/// One rollup row (TASK-016, slice 10.7). The grain depends on the
/// request's `group_by`: `campaign` populates both `platform` and
/// `campaign_id`; `platform` populates only `platform`; `tenant` leaves
/// both `None`. Tenant is supplied by the authenticated route scope.
///
/// `conversion_value` (the platform's own attribution) and `sanvi_revenue`
/// (Sanvi's order-based view) are always both present, always labelled
/// separately — never blended into one number (NFR-1006). Same for
/// `roas_platform`/`roas_sanvi`: each is `null` on zero spend, never `∞`.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct MetricPoint {
    #[schema(example = "google_ads")]
    pub platform: Option<String>,
    #[schema(value_type = Option<String>, format = Uuid)]
    pub campaign_id: Option<String>,
    #[schema(value_type = String, format = Date, example = "2026-08-24")]
    pub date: String,
    pub impressions: i64,
    pub clicks: i64,
    pub spend: MoneyView,
    pub conversions: f64,
    /// Platform-reported conversion value — attribution the ad platform's
    /// own model made.
    pub conversion_value: MoneyView,
    /// Sanvi's own order-based revenue view. Never summed with
    /// `conversion_value` — see the type's doc comment.
    pub sanvi_revenue: MoneyView,
    pub roas_platform: Option<f64>,
    pub roas_sanvi: Option<f64>,
    /// Still inside the platform's restatement window — this day's numbers
    /// may still change on a later ingest.
    pub restating: bool,
    pub rendered_spend: Option<RenderedMoneyView>,
}

impl From<crate::domain::MetricsAggregateRow> for MetricPoint {
    fn from(row: crate::domain::MetricsAggregateRow) -> Self {
        let roas_platform = row.roas_platform();
        let roas_sanvi = row.roas_sanvi();
        Self {
            platform: row.platform.map(|key| key.as_str().to_string()),
            campaign_id: row.campaign_id.map(|id| id.to_string()),
            date: row.date.format("%Y-%m-%d").to_string(),
            impressions: row.impressions,
            clicks: row.clicks,
            spend: MoneyView {
                amount_minor: row.spend_minor,
                currency: row.currency.clone(),
            },
            conversions: row.conversions,
            conversion_value: MoneyView {
                amount_minor: row.conversion_value_minor,
                currency: row.currency.clone(),
            },
            sanvi_revenue: MoneyView {
                amount_minor: row.sanvi_revenue_minor,
                currency: row.currency,
            },
            roas_platform,
            roas_sanvi,
            restating: row.restating,
            // No FX data source is wired up this slice (see
            // `adapters::fx_rate`) — always omitted rather than guessed.
            rendered_spend: None,
        }
    }
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

pub(super) fn api_error(error: AdvertisingError) -> ApiError {
    match error {
        AdvertisingError::InvalidInput(detail) => ApiError::BadRequest { detail },
        AdvertisingError::PlatformUnavailable(detail) => ApiError::ServiceUnavailable { detail },
        AdvertisingError::Storage(detail) => {
            tracing::error!(error = %detail, "advertising drift resolution storage failure");
            ApiError::from_storage_detail(&detail)
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
        AdvertisingError::CreativeNotFound => ApiError::NotFound {
            resource: "creative".to_string(),
        },
        AdvertisingError::ConversionNotFound => ApiError::NotFound {
            resource: "conversion".to_string(),
        },
        AdvertisingError::AudienceNotFound => ApiError::NotFound {
            resource: "audience".to_string(),
        },
        AdvertisingError::BudgetCapNotFound => ApiError::NotFound {
            resource: "budget_cap".to_string(),
        },
        AdvertisingError::BudgetAlertNotFound => ApiError::NotFound {
            resource: "budget_alert".to_string(),
        },
        AdvertisingError::CapBelowCurrentSpend => ApiError::ConflictReasoned {
            detail: "new budget cap is below current period spend and requires confirmation"
                .to_string(),
            reason: "cap_below_current_spend".to_string(),
        },
        AdvertisingError::CapVersionRequired => ApiError::ConflictReasoned {
            detail: "updating an existing budget cap requires an If-Match header carrying the cap's current version; omit If-Match only when creating a cap where none exists".to_string(),
            reason: "cap_version_required".to_string(),
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
        AdvertisingError::ConnectionTokenMissing => ApiError::Conflict {
            detail: "advertising connection has no stored credential; reconnect it".to_string(),
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
        AdvertisingError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "advertising id generation failure");
            ApiError::ServiceUnavailable {
                detail: "advertising service temporarily unavailable".to_string(),
            }
        }
    }
}

/// Error mapping for the budget-cap write endpoints. The generic
/// [`api_error`] renders a lost `StaleRevision` race as campaign drift, but
/// a client that lost a cap-write race must be told to reload the cap, not
/// to resolve campaign drift — so the cap paths re-map that one variant (the
/// other two cap rejections carry their stable `error_kind()` as the
/// problem's machine-readable `reason` from the generic map already, which
/// is what lets a client tell missing-precondition, lost-race, and
/// below-current-spend apart without parsing `detail`).
pub(super) fn cap_api_error(error: AdvertisingError) -> ApiError {
    match error {
        AdvertisingError::StaleRevision => ApiError::ConflictReasoned {
            detail: "budget cap changed since it was read; reload the cap and retry with the current version".to_string(),
            reason: "stale_revision".to_string(),
        },
        other => api_error(other),
    }
}

async fn require_platform_entitlement(
    state: &AdvertisingState,
    tenant_id: TenantId,
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
    tenant_id: TenantId,
    campaign_id: CampaignId,
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

#[derive(Debug, Serialize, ToSchema)]
pub struct CampaignReviewStatusView {
    pub campaign_id: String,
    pub ads: Vec<crate::application::AdReviewStatusView>,
    /// What the platform listed that this campaign's tree cannot account
    /// for. Non-empty means the poll could not fully correlate — an
    /// all-pending `ads` list alongside platform-listed ids here is a
    /// correlation gap, not a clean "everything still in review".
    pub unmatched_platform_ads: crate::application::UnmatchedPlatformAds,
}

/// Refresh every ad's review verdict from the platform. Safe to repeat: it is
/// an observation with a persisted outcome, never a mutation of the platform.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/review-status",
    tag = "advertising",
    operation_id = "poll_campaign_review_status",
    params(("campaign_id" = String, Path, description = "Campaign UUID")),
    responses(
        (status = 200, description = "Fresh per-ad review verdicts; rejection wording is the platform's own", body = CampaignReviewStatusView),
        (status = 400, description = "Campaign not published yet — review has no meaning for a draft", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 404, description = "Campaign or connection not found", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn poll_campaign_review_status(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CampaignId>,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let campaign_id = path.into_inner();
    let (ads, unmatched_platform_ads) = state
        .poll_review_status
        .execute(tenant.0.tenant_id, campaign_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(CampaignReviewStatusView {
        campaign_id: campaign_id.to_string(),
        ads,
        unmatched_platform_ads,
    }))
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
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
    path: web::Path<CampaignId>,
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
    #[schema(value_type = String)]
    pub id: CampaignId,
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
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
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
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

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AdsPageQuery {
    /// Page size, clamped to 1..=200.
    #[param(schema_with = ads_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last item of the previous page. Absent for the
    /// first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn ads_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 200).build()
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/campaigns",
    tag = "advertising",
    operation_id = "list_campaigns",
    params(AdsPageQuery),
    responses(
        (status = 200, description = "One page of the tenant's campaigns, newest first", body = CampaignsView),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_campaigns(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    query: web::Query<AdsPageQuery>,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let after_id = parse_cursor(&query.after)?;
    let campaigns = state
        .list_campaigns
        .execute(tenant.0.tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(CampaignsView {
        campaigns: campaigns.into_iter().map(CampaignView::from).collect(),
    }))
}

fn parse_cursor(after: &Option<String>) -> Result<Option<i64>, ApiError> {
    after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}",
    tag = "advertising",
    operation_id = "get_campaign",
    params(("campaign_id" = String, Path, description = "Campaign UUID")),
    responses(
        (status = 200, description = "The campaign's current intent and revision", body = CampaignView),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail)
    )
)]
pub async fn get_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CampaignId>,
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision; a stale value returns 409 with the current state."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Intent updated; if published, the edit was also pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers, invalid creative id, or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign or creative not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn patch_campaign(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<CampaignId>,
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
    params(("campaign_id" = String, Path, description = "Campaign UUID")),
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
    path: web::Path<CampaignId>,
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
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
    path: web::Path<CampaignId>,
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
        .execute_with_safety(tenant_id, campaign_id, mutation_id, subject.0.user_id)
        .await;
    let stored = match result {
        Ok(stored) => stored,
        Err(PublishError::Preflight(AdvertisingError::Validation(violations))) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(campaign_validation_problem(violations));
        }
        Err(PublishError::Preflight(error)) => {
            release_idempotent(&state, &key, &scope).await;
            return Err(api_error(error).into());
        }
        Err(PublishError::RemoteMayHaveOccurred(error)) => {
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
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
    path: web::Path<CampaignId>,
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
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
    path: web::Path<CampaignId>,
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
    params(
        ("campaign_id" = String, Path, description = "Campaign UUID"),
        AdsPageQuery
    ),
    responses(
        (status = 200, description = "Newest-first attributed change log: actor, source (sanvi|platform), before/after state", body = CampaignChangesView),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 404, description = "Campaign not found", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_campaign_changes(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CampaignId>,
    query: web::Query<AdsPageQuery>,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let after_id = parse_cursor(&query.after)?;
    let changes = state
        .list_campaign_changes
        .execute(tenant.0.tenant_id, path.into_inner(), query.limit, after_id)
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
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
    path: web::Path<CampaignId>,
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
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
    path: web::Path<(CampaignId, String)>,
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
        ("ad_group_id" = String, Path, description = "Ad group id"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Ad appended; if published, pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers, invalid creative id, or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign, ad group or creative not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn add_ad(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(CampaignId, String)>,
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
        ("campaign_id" = String, Path, description = "Campaign UUID"),
        ("ad_group_id" = String, Path, description = "Ad group id"),
        ("ad_id" = String, Path, description = "Ad id"),
        ("If-Match" = String, Header, description = "Required. The campaign's current revision."),
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original result.")
    ),
    responses(
        (status = 200, description = "Ad updated; if published, pushed to the platform", body = CampaignView),
        (status = 400, description = "Missing/malformed headers, invalid creative id, or invalid fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Campaign, ad group, ad or creative not found", body = ProblemDetail),
        (status = 409, description = "Stale revision or idempotency conflict", body = ProblemDetail),
        (status = 503, description = "Platform adapter unavailable", body = ProblemDetail)
    )
)]
pub async fn update_ad(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<(CampaignId, String, String)>,
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
    #[schema(value_type = String)]
    pub id: ConnectionId,
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
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
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
    #[schema(value_type = String)]
    pub id: ConnectionId,
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
    let health = state
        .list_connections
        .health(&connection, chrono::Utc::now())
        .map_err(api_error)?;
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
        .execute(tenant_id)
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
    params(("connection_id" = String, Path, description = "Connection UUID")),
    responses(
        (status = 204, description = "Disconnected in Sanvi; the platform side is untouched"),
        (status = 403, description = "Missing advertising.connect permission or fresh MFA missing", body = ProblemDetail),
        (status = 404, description = "Connection not found", body = ProblemDetail)
    )
)]
pub async fn delete_connection(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<ConnectionId>,
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreativeView {
    pub id: String,
    pub connection_id: String,
    pub platform: String,
    pub external_id: Option<String>,
    pub creative: Creative,
    #[serde(default)]
    pub assets: Vec<AssetMetadata>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<StoredCreative> for CreativeView {
    fn from(stored: StoredCreative) -> Self {
        Self {
            id: stored.id.to_string(),
            connection_id: stored.connection_id.to_string(),
            platform: stored.platform.as_str().to_string(),
            external_id: stored.external_id,
            creative: stored.creative,
            assets: stored.assets,
            created_at: stored.created_at,
            updated_at: stored.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreativesView {
    pub creatives: Vec<CreativeView>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateCreativeRequest {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub placement: String,
    pub texts: Vec<CreativeText>,
    pub asset_references: Vec<String>,
    #[serde(default)]
    pub assets: Vec<AssetMetadata>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CreativePreviewsQuery {
    pub placement: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreativePreviewView {
    pub placement: String,
    pub headline: Option<String>,
    pub body: Option<String>,
    pub asset_references: Vec<String>,
    pub spec: CreativeAssetSpec,
}

impl From<PlacementPreview> for CreativePreviewView {
    fn from(preview: PlacementPreview) -> Self {
        Self {
            placement: preview.placement,
            headline: preview.headline,
            body: preview.body,
            asset_references: preview.asset_references,
            spec: preview.spec,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CreativePreviewsView {
    pub previews: Vec<CreativePreviewView>,
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/creatives",
    tag = "advertising",
    operation_id = "create_creative",
    request_body = CreateCreativeRequest,
    params(
        ("Idempotency-Key" = Uuid, Header, description = "Required. A retry with the same key returns the original creative.")
    ),
    responses(
        (status = 201, description = "Creative created with upload-time matrix validation.", body = CreativeView),
        (status = 400, description = "Missing/malformed Idempotency-Key or invalid creative fields (advertising/campaign-invalid carries field violations)", body = ProblemDetail),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Connection not found", body = ProblemDetail),
        (status = 409, description = "Connection is not active", body = ProblemDetail)
    )
)]
pub async fn create_creative(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _subject: CurrentSubject,
    req: HttpRequest,
    body: web::Json<CreateCreativeRequest>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let mutation_id = idempotency_key(&req)?;
    let command = body.into_inner();
    let platform = state
        .create_creative
        .platform_for_connection(tenant_id, command.connection_id)
        .await
        .map_err(api_error)?;
    require_platform_entitlement(&state, tenant_id, &platform, feature_gate.get_ref()).await?;
    let scope = format!("tenant.{tenant_id}.advertising.creatives.create");
    let key = mutation_id.to_string();
    if let Some(view) = claim_idempotent::<CreativeView>(&state, &key, &scope).await? {
        return Ok(HttpResponse::Created().json(view));
    }

    let result = state
        .create_creative
        .execute(
            tenant_id,
            NewCreativeFields {
                connection_id: command.connection_id,
                placement: command.placement,
                texts: command.texts,
                asset_references: command.asset_references,
                assets: command.assets,
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
    let view = CreativeView::from(stored);
    resolve_idempotent(&state, &key, &scope, &view).await;
    Ok(HttpResponse::Created().json(view))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/creatives/{creative_id}",
    tag = "advertising",
    operation_id = "get_creative",
    params(("creative_id" = String, Path, description = "Creative UUID")),
    responses(
        (status = 200, description = "Stored creative specification and metadata", body = CreativeView),
        (status = 403, description = "Missing advertising.read permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Creative not found", body = ProblemDetail)
    )
)]
pub async fn get_creative(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CreativeId>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let creative_id = path.into_inner();
    let stored = state
        .get_creative
        .execute(tenant_id, creative_id)
        .await
        .map_err(api_error)?
        .ok_or(ApiError::NotFound {
            resource: "creative".to_string(),
        })?;
    require_platform_entitlement(&state, tenant_id, &stored.platform, feature_gate.get_ref())
        .await?;
    Ok(HttpResponse::Ok().json(CreativeView::from(stored)))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/creatives",
    tag = "advertising",
    operation_id = "list_creatives",
    params(AdsPageQuery),
    responses(
        (status = 200, description = "One page of the tenant's creatives on entitled platforms", body = CreativesView),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission or advertising platform entitlement", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_creatives(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    query: web::Query<AdsPageQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let after_id = parse_cursor(&query.after)?;
    let creatives = state
        .list_creatives
        .execute(tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    // One gate check per distinct platform — per-creative checks resolved
    // the same (tenant, platform) answer up to page-size times (TASK-021
    // 11.2e). The catalog itself is in-memory.
    let catalog = state.catalog.execute();
    let mut entitled: std::collections::HashMap<crate::domain::PlatformKey, bool> =
        std::collections::HashMap::new();
    for creative in &creatives {
        if !entitled.contains_key(&creative.platform) {
            let descriptor = catalog
                .iter()
                .find(|descriptor| descriptor.key == creative.platform)
                .ok_or_else(|| ApiError::ServiceUnavailable {
                    detail: "advertising platform is unavailable".to_string(),
                })?;
            let allowed = feature_gate
                .check(tenant_id, &descriptor.entitlement_key)
                .await?;
            entitled.insert(creative.platform.clone(), allowed);
        }
    }
    let visible = creatives
        .into_iter()
        .filter(|creative| entitled.get(&creative.platform).copied().unwrap_or(false))
        .map(CreativeView::from)
        .collect();
    Ok(HttpResponse::Ok().json(CreativesView { creatives: visible }))
}

#[utoipa::path(
    delete,
    path = "/api/v1/tenant/ads/creatives/{creative_id}",
    tag = "advertising",
    operation_id = "delete_creative",
    params(("creative_id" = String, Path, description = "Creative UUID")),
    responses(
        (status = 204, description = "Creative deleted"),
        (status = 403, description = "Missing advertising.campaign.write permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Creative not found", body = ProblemDetail)
    )
)]
pub async fn delete_creative(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CreativeId>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let creative_id = path.into_inner();
    let stored = state
        .get_creative
        .execute(tenant_id, creative_id)
        .await
        .map_err(api_error)?
        .ok_or(ApiError::NotFound {
            resource: "creative".to_string(),
        })?;
    require_platform_entitlement(&state, tenant_id, &stored.platform, feature_gate.get_ref())
        .await?;
    state
        .delete_creative
        .execute(tenant_id, creative_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::NoContent().finish())
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/creatives/{creative_id}/previews",
    tag = "advertising",
    operation_id = "get_creative_previews",
    params(
        ("creative_id" = String, Path, description = "Creative UUID"),
        CreativePreviewsQuery
    ),
    responses(
        (status = 200, description = "Spec-rendered placement previews", body = CreativePreviewsView),
        (status = 403, description = "Missing advertising.read permission or platform entitlement", body = ProblemDetail),
        (status = 404, description = "Creative not found", body = ProblemDetail)
    )
)]
pub async fn get_creative_previews(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CreativeId>,
    query: web::Query<CreativePreviewsQuery>,
    _permission: RequiredPermission,
    _entitlement: RequiredAnyFeature,
    feature_gate: web::Data<Arc<dyn sanvi_http::FeatureGate>>,
) -> Result<HttpResponse, actix_web::Error> {
    let tenant_id = tenant.0.tenant_id;
    let creative_id = path.into_inner();
    let stored = state
        .get_creative
        .execute(tenant_id, creative_id)
        .await
        .map_err(api_error)?
        .ok_or(ApiError::NotFound {
            resource: "creative".to_string(),
        })?;
    require_platform_entitlement(&state, tenant_id, &stored.platform, feature_gate.get_ref())
        .await?;
    let previews = state
        .get_creative_previews
        .execute(tenant_id, creative_id, query.into_inner().placement)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(CreativePreviewsView {
        previews: previews
            .into_iter()
            .map(CreativePreviewView::from)
            .collect(),
    }))
}

// ---------------------------------------------------------------------
// Metrics ingestion, rollups & the ROAS query layer (TASK-016, slice 10.7)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MetricsQueryParams {
    /// Inclusive range start, `YYYY-MM-DD`.
    #[param(example = "2026-08-01")]
    pub from: String,
    /// Inclusive range end, `YYYY-MM-DD`.
    #[param(example = "2026-08-24")]
    pub to: String,
    /// `campaign` (default), `platform`, or `tenant`.
    #[param(example = "campaign")]
    pub group_by: Option<String>,
    #[param(example = "google_ads")]
    pub platform: Option<String>,
    pub campaign_id: Option<String>,
    /// Optional display-only currency for `rendered_spend`. Conversion is
    /// never persisted and never replaces the native `spend` — each rendered
    /// value carries its FX rate date. Omit to keep account currencies only.
    #[param(example = "JPY")]
    pub render_currency: Option<String>,
}

/// The CSV export's slice of the metrics query: everything
/// [`MetricsQueryParams`] carries except `render_currency`, because the
/// export renders only native-currency figures. Publishing the shared struct
/// here advertised a parameter the handler silently ignored — native-currency
/// CSV with no signal that a requested conversion never happened.
/// `deny_unknown_fields` turns a stray `render_currency` into a 400 instead.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct MetricsExportQueryParams {
    /// Inclusive range start, `YYYY-MM-DD`.
    #[param(example = "2026-08-01")]
    pub from: String,
    /// Inclusive range end, `YYYY-MM-DD`.
    #[param(example = "2026-08-24")]
    pub to: String,
    /// `campaign` (default), `platform`, or `tenant`.
    #[param(example = "campaign")]
    pub group_by: Option<String>,
    #[param(example = "google_ads")]
    pub platform: Option<String>,
    pub campaign_id: Option<String>,
}

fn parse_group_by(raw: Option<&str>) -> Result<MetricsGroupBy, ApiError> {
    match raw {
        None | Some("campaign") => Ok(MetricsGroupBy::Campaign),
        Some("platform") => Ok(MetricsGroupBy::Platform),
        Some("tenant") => Ok(MetricsGroupBy::Tenant),
        Some(other) => Err(ApiError::BadRequest {
            detail: format!("unknown group_by {other:?}; expected campaign, platform, or tenant"),
        }),
    }
}

fn parse_metrics_date(label: &str, raw: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|_| ApiError::BadRequest {
        detail: format!("{label} must be a date in YYYY-MM-DD form"),
    })
}

fn parse_metrics_query(
    tenant_id: TenantId,
    params: MetricsQueryParams,
) -> Result<MetricsRangeQuery, ApiError> {
    let from = parse_metrics_date("from", &params.from)?;
    let to = parse_metrics_date("to", &params.to)?;
    let group_by = parse_group_by(params.group_by.as_deref())?;
    let platform = params
        .platform
        .map(|raw| PlatformKey::parse(&raw))
        .transpose()
        .map_err(api_error)?;
    let campaign_id = params
        .campaign_id
        .map(|raw| raw.parse::<CampaignId>())
        .transpose()
        .map_err(|_| ApiError::BadRequest {
            detail: "campaign_id must be a valid campaign id".to_string(),
        })?;
    Ok(MetricsRangeQuery {
        tenant_id,
        from,
        to,
        group_by,
        platform,
        campaign_id,
    })
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetricsQueryResponse {
    pub rows: Vec<MetricPoint>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/metrics",
    tag = "advertising",
    operation_id = "get_ads_metrics",
    params(MetricsQueryParams),
    responses(
        (status = 200, description = "Rollup rows for the requested range and grain, both ROAS numbers labelled separately", body = MetricsQueryResponse),
        (status = 400, description = "Invalid date range, unknown group_by, or a span exceeding the configured maximum", body = ProblemDetail),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail)
    )
)]
pub async fn get_ads_metrics(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    query: web::Query<MetricsQueryParams>,
) -> Result<HttpResponse, ApiError> {
    let params = query.into_inner();
    let render_currency = match params.render_currency.as_deref() {
        None => None,
        Some(raw) => Some(
            Money::new(0, raw)
                .map_err(|_| ApiError::BadRequest {
                    detail: "render_currency must be an ISO-4217 currency code".to_string(),
                })?
                .currency,
        ),
    };
    let range = parse_metrics_query(tenant.0.tenant_id, params)?;
    let today = Utc::now().date_naive();
    let rows = state
        .query_metrics
        .execute(range, today)
        .await
        .map_err(api_error)?;
    let mut points: Vec<MetricPoint> = rows.into_iter().map(MetricPoint::from).collect();
    if let Some(target) = &render_currency {
        resolve_rendered_spend(state.fx_rates.as_ref(), target, &mut points).await;
    }
    Ok(HttpResponse::Ok().json(MetricsQueryResponse { rows: points }))
}

/// Fill in every row's `rendered_spend` at query time. Rates are resolved
/// once per distinct (source currency, day) pair before any row is
/// converted — the persisted FX port is a database round-trip per lookup
/// plus outbound provider HTTP on a snapshot miss, so per-row resolution on
/// a 90-day × N-campaign response meant up to 90×N serial queries and just
/// as many cold external fetches. A day without a published rate keeps
/// `rendered_spend: None` — an omitted value is honest, a guessed rate is
/// not. A *failing* rate store is not "no rate that day": the error is
/// logged per affected pair instead of collapsing into the same silent
/// `None`. Nothing here is persisted (NFR: converted money is display-only
/// and always labelled with its rate date).
async fn resolve_rendered_spend(
    fx: &dyn crate::domain::FxRatePort,
    target: &str,
    points: &mut [MetricPoint],
) {
    use std::collections::{BTreeSet, HashMap};

    // Same-currency rows need no rate (1.0 by convention); collect the
    // remaining (currency, date) pairs exactly once each. Rows whose date
    // does not parse keep `rendered_spend: None`, as before.
    let pairs: BTreeSet<(String, String)> = points
        .iter()
        .filter(|point| point.spend.currency != target)
        .filter_map(|point| {
            NaiveDate::parse_from_str(&point.date, "%Y-%m-%d").ok()?;
            Some((point.spend.currency.clone(), point.date.clone()))
        })
        .collect();

    let mut rates: HashMap<(String, String), crate::domain::FxRate> =
        HashMap::with_capacity(pairs.len());
    for (currency, date_label) in &pairs {
        let Ok(spend_date) = NaiveDate::parse_from_str(date_label, "%Y-%m-%d") else {
            continue;
        };
        match fx.rate(currency, target, spend_date).await {
            Ok(Some(rate)) => {
                rates.insert((currency.clone(), date_label.clone()), rate);
            }
            // No published rate that day: leave those rows unconverted.
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    %error,
                    currency = %currency,
                    date = %date_label,
                    "FX lookup failed; affected rows render without converted spend"
                );
            }
        }
    }

    for point in points.iter_mut() {
        if point.spend.currency == target {
            point.rendered_spend = Some(RenderedMoneyView {
                amount_minor: point.spend.amount_minor,
                currency: target.to_string(),
                fx_rate_date: point.date.clone(),
            });
            continue;
        }
        let Some(rate) = rates.get(&(point.spend.currency.clone(), point.date.clone())) else {
            continue;
        };
        let source_exponent = 10_f64.powi(Money::exponent(&point.spend.currency) as i32);
        let target_exponent = 10_f64.powi(Money::exponent(target) as i32);
        let converted_major = point.spend.amount_minor as f64 / source_exponent * rate.rate;
        point.rendered_spend = Some(RenderedMoneyView {
            amount_minor: (converted_major * target_exponent).round() as i64,
            currency: target.to_string(),
            fx_rate_date: rate.date.format("%Y-%m-%d").to_string(),
        });
    }
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MetricsSummaryQueryParams {
    #[param(example = "2026-08-01")]
    pub from: String,
    #[param(example = "2026-08-24")]
    pub to: String,
    /// Start date of an equal-length prior period to compare against
    /// (`YYYY-MM-DD`). Omit for no comparison.
    #[param(example = "2026-07-01")]
    pub compare_to: Option<String>,
}

/// One currency's totals across the whole requested range — the range
/// itself collapsed, so unlike [`MetricPoint`] there is one row per
/// currency, not per day.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct MetricsSummaryRow {
    pub currency: String,
    pub impressions: i64,
    pub clicks: i64,
    pub spend: MoneyView,
    pub conversions: f64,
    pub conversion_value: MoneyView,
    pub sanvi_revenue: MoneyView,
    pub roas_platform: Option<f64>,
    pub roas_sanvi: Option<f64>,
    /// `true` if any day in the range is still inside its platform's
    /// restatement window.
    pub restating: bool,
}

fn summarize(rows: &[crate::domain::MetricsAggregateRow]) -> Vec<MetricsSummaryRow> {
    use std::collections::BTreeMap;
    let mut by_currency: BTreeMap<String, MetricsSummaryRow> = BTreeMap::new();
    for row in rows {
        let entry = by_currency
            .entry(row.currency.clone())
            .or_insert_with(|| MetricsSummaryRow {
                currency: row.currency.clone(),
                impressions: 0,
                clicks: 0,
                spend: MoneyView {
                    amount_minor: 0,
                    currency: row.currency.clone(),
                },
                conversions: 0.0,
                conversion_value: MoneyView {
                    amount_minor: 0,
                    currency: row.currency.clone(),
                },
                sanvi_revenue: MoneyView {
                    amount_minor: 0,
                    currency: row.currency.clone(),
                },
                roas_platform: None,
                roas_sanvi: None,
                restating: false,
            });
        entry.impressions += row.impressions;
        entry.clicks += row.clicks;
        entry.spend.amount_minor += row.spend_minor;
        entry.conversions += row.conversions;
        entry.conversion_value.amount_minor += row.conversion_value_minor;
        entry.sanvi_revenue.amount_minor += row.sanvi_revenue_minor;
        entry.restating = entry.restating || row.restating;
    }
    for row in by_currency.values_mut() {
        row.roas_platform = (row.spend.amount_minor != 0)
            .then_some(row.conversion_value.amount_minor as f64 / row.spend.amount_minor as f64);
        row.roas_sanvi = (row.spend.amount_minor != 0)
            .then_some(row.sanvi_revenue.amount_minor as f64 / row.spend.amount_minor as f64);
    }
    by_currency.into_values().collect()
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetricsSummaryResponse {
    pub current: Vec<MetricsSummaryRow>,
    pub compared: Option<Vec<MetricsSummaryRow>>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/metrics/summary",
    tag = "advertising",
    operation_id = "get_ads_metrics_summary",
    params(MetricsSummaryQueryParams),
    responses(
        (status = 200, description = "Whole-range totals per currency, with an optional prior-period comparison", body = MetricsSummaryResponse),
        (status = 400, description = "Invalid date range or a span exceeding the configured maximum", body = ProblemDetail),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail)
    )
)]
pub async fn get_ads_metrics_summary(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    query: web::Query<MetricsSummaryQueryParams>,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let params = query.into_inner();
    let from = parse_metrics_date("from", &params.from)?;
    let to = parse_metrics_date("to", &params.to)?;
    let today = Utc::now().date_naive();

    let current_range = MetricsRangeQuery {
        tenant_id,
        from,
        to,
        group_by: MetricsGroupBy::Tenant,
        platform: None,
        campaign_id: None,
    };
    let current = state
        .query_metrics
        .execute(current_range, today)
        .await
        .map_err(api_error)?;

    let compared = match params.compare_to {
        Some(raw) => {
            let compare_from = parse_metrics_date("compare_to", &raw)?;
            let span_days = (to - from).num_days();
            let Some(compare_to) =
                compare_from.checked_add_signed(chrono::Duration::days(span_days))
            else {
                return Err(ApiError::BadRequest {
                    detail: "compare_to produces an out-of-range comparison period".to_string(),
                });
            };
            let compare_range = MetricsRangeQuery {
                tenant_id,
                from: compare_from,
                to: compare_to,
                group_by: MetricsGroupBy::Tenant,
                platform: None,
                campaign_id: None,
            };
            let rows = state
                .query_metrics
                .execute(compare_range, today)
                .await
                .map_err(api_error)?;
            Some(summarize(&rows))
        }
        None => None,
    };

    Ok(HttpResponse::Ok().json(MetricsSummaryResponse {
        current: summarize(&current),
        compared,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/metrics/export",
    tag = "advertising",
    operation_id = "get_ads_metrics_export",
    params(MetricsExportQueryParams),
    responses(
        (status = 200, description = "The same labelled columns as /ads/metrics, streamed as CSV — no blended ROAS column", content_type = "text/csv"),
        (status = 400, description = "Invalid date range, unknown group_by, an unsupported query parameter (e.g. render_currency), or a span exceeding the configured maximum", body = ProblemDetail),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail)
    )
)]
pub async fn get_ads_metrics_export(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    query: web::Query<MetricsExportQueryParams>,
) -> Result<HttpResponse, ApiError> {
    let range = parse_metrics_query(
        tenant.0.tenant_id,
        MetricsQueryParams {
            from: query.from.clone(),
            to: query.to.clone(),
            group_by: query.group_by.clone(),
            platform: query.platform.clone(),
            campaign_id: query.campaign_id.clone(),
            render_currency: None,
        },
    )?;
    let today = Utc::now().date_naive();
    let rows = state
        .query_metrics
        .execute(range, today)
        .await
        .map_err(api_error)?;

    let mut csv = String::from(
        "platform,campaign_id,date,impressions,clicks,spend_minor,currency,conversions,\
         conversion_value_minor,sanvi_revenue_minor,roas_platform,roas_sanvi,restating\n",
    );
    for row in rows.into_iter().map(MetricPoint::from) {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            row.platform.unwrap_or_default(),
            row.campaign_id.unwrap_or_default(),
            row.date,
            row.impressions,
            row.clicks,
            row.spend.amount_minor,
            row.spend.currency,
            row.conversions,
            row.conversion_value.amount_minor,
            row.sanvi_revenue.amount_minor,
            row.roas_platform.map(|v| v.to_string()).unwrap_or_default(),
            row.roas_sanvi.map(|v| v.to_string()).unwrap_or_default(),
            row.restating,
        ));
    }

    Ok(HttpResponse::Ok()
        .content_type("text/csv")
        .insert_header((
            "content-disposition",
            "attachment; filename=\"ads-metrics.csv\"",
        ))
        .body(csv))
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConnectionFreshnessView {
    #[schema(value_type = String)]
    pub connection_id: ConnectionId,
    pub platform: String,
    pub last_ingested_at: Option<DateTime<Utc>>,
    pub lag_hours: Option<f64>,
    /// Ingestion is older than the configured freshness threshold — the
    /// dashboard's "sync failed" / "still updating" state, never presented
    /// as current.
    pub stalled: bool,
}

impl From<ConnectionFreshness> for ConnectionFreshnessView {
    fn from(freshness: ConnectionFreshness) -> Self {
        Self {
            connection_id: freshness.connection_id,
            platform: freshness.platform.as_str().to_string(),
            last_ingested_at: freshness.last_ingested_at,
            lag_hours: freshness.lag_hours,
            stalled: freshness.stalled,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MetricsFreshnessView {
    pub connections: Vec<ConnectionFreshnessView>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/metrics/freshness",
    tag = "advertising",
    operation_id = "get_ads_metrics_freshness",
    responses(
        (status = 200, description = "Per-connection last-ingested-at and lag — what a stalled sync looks like, not a guess based on the clock", body = MetricsFreshnessView),
        (status = 403, description = "Missing advertising.metrics.read permission", body = ProblemDetail)
    )
)]
pub async fn get_ads_metrics_freshness(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let connections = state
        .get_freshness
        .execute(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(MetricsFreshnessView {
        connections: connections
            .into_iter()
            .map(ConnectionFreshnessView::from)
            .collect(),
    }))
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BudgetCapsView {
    pub caps: Vec<BudgetCap>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BudgetAlertsView {
    pub alerts: Vec<BudgetAlert>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PutBudgetCapRequest {
    pub period: BudgetPeriod,
    pub amount: Money,
    pub declared_fx_basis: Option<String>,
    pub fx_rate_date: Option<NaiveDate>,
    pub auto_pause: Option<bool>,
    pub auto_resume_on_rollover: Option<bool>,
    pub confirm_below_current_spend: Option<bool>,
    pub dry_run: Option<bool>,
}

impl PutBudgetCapRequest {
    pub fn into_command(self, expected_version: Option<i64>) -> PutBudgetCapCommand {
        PutBudgetCapCommand {
            period: self.period,
            amount: self.amount,
            declared_fx_basis: self.declared_fx_basis,
            fx_rate_date: self.fx_rate_date,
            auto_pause: self.auto_pause.unwrap_or(false),
            auto_resume_on_rollover: self.auto_resume_on_rollover.unwrap_or(false),
            confirm_below_current_spend: self.confirm_below_current_spend.unwrap_or(false),
            dry_run: self.dry_run.unwrap_or(false),
            expected_version,
        }
    }
}

/// Optimistic-locking guard for budget-cap writes. The header stays optional
/// because creates and dry runs may omit `If-Match`, but updating an
/// existing cap without it is rejected (`CapVersionRequired` → 409, enforced
/// in `PutBudgetCap`); a stale version surfaces as 409 via `StaleRevision`.
/// The version is read in RFC 7232 entity-tag form — `If-Match: "17"` — or
/// as the bare integer earlier clients sent; both reach the use case as
/// `17`. `If-Match: *` is refused with a 400: this endpoint creates or
/// updates, so "any existing entity" cannot say which, and a blind
/// create-over-update is exactly what the precondition exists to prevent.
fn optional_if_match_version(req: &HttpRequest) -> Result<Option<i64>, ApiError> {
    let raw = req
        .headers()
        .get("If-Match")
        .and_then(|value| value.to_str().ok());
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw == "*" {
        return Err(ApiError::BadRequest {
            detail: "If-Match: * is not supported for budget caps; send the version you read, as a quoted entity tag (e.g. \"17\")".to_string(),
        });
    }
    let bad = || {
        ApiError::BadRequest {
        detail: "If-Match must be the cap's version as a quoted entity tag (e.g. \"17\") or a bare integer".to_string(),
    }
    };
    let version = if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    version.parse().map(Some).map_err(|_| bad())
}

/// A cap write that applied carries the new version as a strong ETag, so the
/// client has the exact value to echo in the next write's `If-Match`. Dry
/// runs changed nothing and carry none.
fn cap_response(result: PutBudgetCapResponse) -> Result<HttpResponse, ApiError> {
    match &result {
        PutBudgetCapResponse::Applied(cap) => Ok(HttpResponse::Ok()
            .insert_header((
                actix_web::http::header::ETAG,
                format!("\"{}\"", cap.version),
            ))
            .json(result)),
        PutBudgetCapResponse::DryRun(_) => Ok(HttpResponse::Ok().json(result)),
    }
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct GetCampaignBudgetCapQuery {
    pub period: Option<BudgetPeriod>,
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListBudgetAlertsQuery {
    #[param(value_type = Option<String>)]
    pub campaign_id: Option<CampaignId>,
    pub unacknowledged_only: Option<bool>,
    /// Page size, clamped to 1..=200.
    #[param(schema_with = budget_alert_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last alert of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn budget_alert_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 50).build()
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SpendStatusQuery {
    pub period: Option<BudgetPeriod>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/budget-caps",
    tag = "advertising",
    operation_id = "get_budget_caps",
    params(AdsPageQuery),
    responses(
        (status = 200, description = "One page of the budget caps defined for this tenant", body = BudgetCapsView),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn get_budget_caps(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    query: web::Query<AdsPageQuery>,
    _permission: RequiredPermission,
    _feature: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let after_id = parse_cursor(&query.after)?;
    let caps = state
        .get_budget_caps
        .execute(tenant.0.tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(BudgetCapsView { caps }))
}

#[utoipa::path(
    put,
    path = "/api/v1/tenant/ads/budget-caps",
    tag = "advertising",
    operation_id = "put_budget_cap",
    params(
        ("If-Match" = Option<String>, Header, description = "Conditional. Required when updating an existing cap — a version-less update is rejected with 409 (`reason: cap_version_required`). Omitting it is valid only for creates (no cap exists yet for this scope and period) and for dry runs. The version is a quoted entity tag (e.g. \"17\") or the bare integer; `If-Match: *` is not accepted. A stale value returns 409 (`reason: stale_revision`).")
    ),
    request_body = PutBudgetCapRequest,
    responses(
        (status = 200, description = "Budget cap created or updated (an applied write carries an `ETag` header with the new version), or dry-run simulation result", body = PutBudgetCapResponse),
        (status = 403, description = "Missing advertising.budget.manage permission", body = ProblemDetail),
        (status = 409, description = "Cap conflict, distinguished by the problem `reason` member: `cap_version_required` (update without If-Match), `stale_revision` (lost version race), `cap_below_current_spend` (lowering below period spend without confirmation)", body = ProblemDetail)
    )
)]
pub async fn put_budget_cap(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    _permission: RequiredPermission,
    _feature: RequiredAnyFeature,
    req: HttpRequest,
    body: web::Json<PutBudgetCapRequest>,
) -> Result<HttpResponse, ApiError> {
    let expected_version = optional_if_match_version(&req)?;
    let result = state
        .put_budget_cap
        .execute(
            tenant.0.tenant_id,
            None,
            body.into_inner().into_command(expected_version),
        )
        .await
        .map_err(cap_api_error)?;
    cap_response(result)
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
    tag = "advertising",
    operation_id = "get_campaign_budget_cap",
    params(
        ("campaign_id" = String, Path, description = "Target campaign ID"),
        GetCampaignBudgetCapQuery
    ),
    responses(
        (status = 200, description = "Campaign budget cap (carries an `ETag` header with the cap version)", body = BudgetCap),
        (status = 404, description = "No budget cap found for this campaign and period", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail)
    )
)]
pub async fn get_campaign_budget_cap(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CampaignId>,
    query: web::Query<GetCampaignBudgetCapQuery>,
    _permission: RequiredPermission,
    _feature: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let campaign_id = path.into_inner();
    let period = query.period.unwrap_or(BudgetPeriod::Monthly);
    let cap = state
        .get_campaign_budget_cap
        .execute(tenant.0.tenant_id, campaign_id, period)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok()
        .insert_header((
            actix_web::http::header::ETAG,
            format!("\"{}\"", cap.version),
        ))
        .json(cap))
}

#[utoipa::path(
    put,
    path = "/api/v1/tenant/ads/campaigns/{campaign_id}/budget-cap",
    tag = "advertising",
    operation_id = "put_campaign_budget_cap",
    params(
        ("campaign_id" = String, Path, description = "Target campaign ID"),
        ("If-Match" = Option<String>, Header, description = "Conditional. Required when updating an existing cap — a version-less update is rejected with 409 (`reason: cap_version_required`). Omitting it is valid only for creates (no cap exists yet for this scope and period) and for dry runs. The version is a quoted entity tag (e.g. \"17\") or the bare integer; `If-Match: *` is not accepted. A stale value returns 409 (`reason: stale_revision`).")
    ),
    request_body = PutBudgetCapRequest,
    responses(
        (status = 200, description = "Campaign budget cap created/updated (an applied write carries an `ETag` header with the new version) or dry run result", body = PutBudgetCapResponse),
        (status = 403, description = "Missing advertising.budget.manage permission", body = ProblemDetail),
        (status = 409, description = "Cap conflict, distinguished by the problem `reason` member: `cap_version_required` (update without If-Match), `stale_revision` (lost version race), `cap_below_current_spend` (lowering below period spend without confirmation)", body = ProblemDetail)
    )
)]
pub async fn put_campaign_budget_cap(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    path: web::Path<CampaignId>,
    _permission: RequiredPermission,
    _feature: RequiredAnyFeature,
    req: HttpRequest,
    body: web::Json<PutBudgetCapRequest>,
) -> Result<HttpResponse, ApiError> {
    let campaign_id = path.into_inner();
    let expected_version = optional_if_match_version(&req)?;
    let result = state
        .put_budget_cap
        .execute(
            tenant.0.tenant_id,
            Some(campaign_id),
            body.into_inner().into_command(expected_version),
        )
        .await
        .map_err(cap_api_error)?;
    cap_response(result)
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/budget-alerts",
    tag = "advertising",
    operation_id = "get_budget_alerts",
    params(
        ListBudgetAlertsQuery
    ),
    responses(
        (status = 200, description = "Budget alerts history", body = BudgetAlertsView),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn get_budget_alerts(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    query: web::Query<ListBudgetAlertsQuery>,
    _permission: RequiredPermission,
    _feature: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let after_id = parse_cursor(&query.after)?;
    let alerts = state
        .list_budget_alerts
        .execute(
            tenant.0.tenant_id,
            query.campaign_id,
            query.unacknowledged_only.unwrap_or(false),
            query.limit,
            after_id,
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(BudgetAlertsView { alerts }))
}

#[utoipa::path(
    post,
    path = "/api/v1/tenant/ads/budget-alerts/{alert_id}/acknowledge",
    tag = "advertising",
    operation_id = "acknowledge_budget_alert",
    params(
        ("alert_id" = String, Path, description = "Target budget alert ID")
    ),
    responses(
        (status = 200, description = "Acknowledged budget alert", body = BudgetAlert),
        (status = 404, description = "Budget alert not found", body = ProblemDetail),
        (status = 403, description = "Missing advertising.budget.manage permission", body = ProblemDetail)
    )
)]
pub async fn acknowledge_budget_alert(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<BudgetAlertId>,
    _permission: RequiredPermission,
    _feature: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let alert_id = path.into_inner();
    let user_id = subject.0.user_id;
    let alert = state
        .acknowledge_budget_alert
        .execute(tenant.0.tenant_id, alert_id, user_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(alert))
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/ads/spend-status",
    tag = "advertising",
    operation_id = "get_spend_status",
    params(
        SpendStatusQuery
    ),
    responses(
        (status = 200, description = "Spend status and run-rate projection across tenant and campaign scopes", body = SpendStatusReport),
        (status = 403, description = "Missing advertising.read permission", body = ProblemDetail)
    )
)]
pub async fn get_spend_status(
    state: web::Data<AdvertisingState>,
    tenant: CurrentTenant,
    query: web::Query<SpendStatusQuery>,
    _permission: RequiredPermission,
    _feature: RequiredAnyFeature,
) -> Result<HttpResponse, ApiError> {
    let report = state
        .get_spend_status
        .execute(tenant.0.tenant_id, query.period)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(report))
}

#[utoipa::path(
    get,
    path = "/api/v1/platform/ads/health",
    tag = "advertising",
    operation_id = "get_platform_ads_health",
    responses(
        (status = 200, description = "Cross-tenant advertising connection health and freshness overview", body = PlatformAdsHealthReport),
        (status = 403, description = "Missing platform.tenant.read permission", body = ProblemDetail)
    )
)]
pub async fn get_platform_ads_health(
    state: web::Data<AdvertisingState>,
    _permission: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let report = state
        .get_platform_ads_health
        .execute()
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(report))
}

/// The four `If-Match` shapes the budget-cap writes accept or refuse — the
/// only tests driving the header anywhere in the repo, so each branch of
/// [`optional_if_match_version`] is pinned here.
#[cfg(test)]
mod if_match_tests {
    use super::*;
    use actix_web::http::header::IF_MATCH;
    use actix_web::test::TestRequest;

    fn request_with_if_match(value: &str) -> HttpRequest {
        TestRequest::default()
            .insert_header((IF_MATCH, value))
            .to_http_request()
    }

    #[test]
    fn a_missing_header_is_none() {
        let req = TestRequest::default().to_http_request();
        assert_eq!(optional_if_match_version(&req).unwrap(), None);
    }

    #[test]
    fn a_quoted_entity_tag_yields_the_version() {
        let req = request_with_if_match("\"17\"");
        assert_eq!(optional_if_match_version(&req).unwrap(), Some(17));
    }

    #[test]
    fn a_bare_integer_yields_the_version() {
        let req = request_with_if_match("17");
        assert_eq!(optional_if_match_version(&req).unwrap(), Some(17));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let req = request_with_if_match(" \"17\" ");
        assert_eq!(optional_if_match_version(&req).unwrap(), Some(17));
    }

    #[test]
    fn the_wildcard_is_refused_as_a_bad_request() {
        let req = request_with_if_match("*");
        match optional_if_match_version(&req) {
            Err(ApiError::BadRequest { detail }) => {
                assert!(detail.contains("not supported"), "detail: {detail}");
            }
            other => panic!("expected BadRequest, got {other:?}"),
        }
    }

    #[test]
    fn a_non_numeric_entity_tag_is_refused() {
        let req = request_with_if_match("\"abc\"");
        assert_eq!(
            optional_if_match_version(&req).unwrap_err().status(),
            actix_web::http::StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn an_empty_entity_tag_is_refused() {
        let req = request_with_if_match("\"\"");
        assert!(matches!(
            optional_if_match_version(&req),
            Err(ApiError::BadRequest { .. })
        ));
    }
}

#[cfg(test)]
mod fx_render_tests {
    use super::*;
    use crate::domain::{AdReviewObservation, AdReviewState};
    use async_trait::async_trait;

    /// Fixed-rate FX stub: every pair converts at 0.5 quote per base.
    struct HalfRateFx;

    #[async_trait]
    impl crate::domain::FxRatePort for HalfRateFx {
        async fn rate(
            &self,
            _base: &str,
            _quote: &str,
            date: NaiveDate,
        ) -> Result<Option<crate::domain::FxRate>, AdvertisingError> {
            Ok(Some(crate::domain::FxRate { rate: 0.5, date }))
        }
        async fn has_snapshot(&self, _date: NaiveDate) -> Result<bool, AdvertisingError> {
            Ok(true)
        }
        async fn published_day(
            &self,
            _date: NaiveDate,
        ) -> Result<Option<Vec<(String, f64)>>, AdvertisingError> {
            Ok(None)
        }
    }

    fn row(currency: &str, amount_minor: i64) -> MetricPoint {
        MetricPoint {
            platform: Some("google_ads".to_string()),
            campaign_id: None,
            date: "2026-09-01".to_string(),
            impressions: 0,
            clicks: 0,
            spend: MoneyView {
                amount_minor,
                currency: currency.to_string(),
            },
            conversions: 0.0,
            conversion_value: MoneyView {
                amount_minor: 0,
                currency: currency.to_string(),
            },
            sanvi_revenue: MoneyView {
                amount_minor: 0,
                currency: currency.to_string(),
            },
            roas_platform: None,
            roas_sanvi: None,
            restating: false,
            rendered_spend: None,
        }
    }

    #[tokio::test]
    async fn rendered_spend_crosses_the_decimal_exponent_boundary() {
        // 1_000 JPY (zero-decimal) at 0.5 => 500.00 USD => 50_000 minor units.
        let mut rows = vec![row("JPY", 1_000)];
        resolve_rendered_spend(&HalfRateFx, "USD", &mut rows).await;
        let rendered = rows[0].rendered_spend.as_ref().expect("JPY->USD renders");
        assert_eq!(rendered.amount_minor, 50_000);
        assert_eq!(rendered.currency, "USD");
        assert_eq!(rendered.fx_rate_date, "2026-09-01");

        // 5_000 USD (2-decimal) at 0.5 => 25.00 of a zero-decimal target => 25 minor units.
        let mut rows = vec![row("USD", 5_000)];
        resolve_rendered_spend(&HalfRateFx, "JPY", &mut rows).await;
        let rendered = rows[0].rendered_spend.as_ref().expect("USD->JPY renders");
        assert_eq!(rendered.amount_minor, 25);
        assert_eq!(rendered.currency, "JPY");

        // Same-currency rows render unchanged and never reach the rate
        // source: the port contract is only asked for base != quote.
        let mut rows = vec![row("EUR", 4_000)];
        resolve_rendered_spend(&HalfRateFx, "EUR", &mut rows).await;
        let rendered = rows[0].rendered_spend.as_ref().expect("EUR->EUR renders");
        assert_eq!(rendered.amount_minor, 4_000);
        assert_eq!(rendered.fx_rate_date, "2026-09-01");

        // A day without a published rate stays None rather than guessing.
        struct NoRate;
        #[async_trait]
        impl crate::domain::FxRatePort for NoRate {
            async fn rate(
                &self,
                _: &str,
                _: &str,
                _: NaiveDate,
            ) -> Result<Option<crate::domain::FxRate>, AdvertisingError> {
                Ok(None)
            }
            async fn has_snapshot(&self, _: NaiveDate) -> Result<bool, AdvertisingError> {
                Ok(false)
            }
            async fn published_day(
                &self,
                _: NaiveDate,
            ) -> Result<Option<Vec<(String, f64)>>, AdvertisingError> {
                Ok(None)
            }
        }
        let mut rows = vec![row("GBP", 1_000)];
        resolve_rendered_spend(&NoRate, "USD", &mut rows).await;
        assert!(rows[0].rendered_spend.is_none());
    }

    /// Rate source that counts how many times it is asked.
    struct CountingFx(std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl crate::domain::FxRatePort for CountingFx {
        async fn rate(
            &self,
            _base: &str,
            _quote: &str,
            date: NaiveDate,
        ) -> Result<Option<crate::domain::FxRate>, AdvertisingError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(crate::domain::FxRate { rate: 1.0, date }))
        }
        async fn has_snapshot(&self, _date: NaiveDate) -> Result<bool, AdvertisingError> {
            Ok(true)
        }
        async fn published_day(
            &self,
            _date: NaiveDate,
        ) -> Result<Option<Vec<(String, f64)>>, AdvertisingError> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn rates_resolve_once_per_currency_and_day() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Three rows, but only two distinct (currency, day) pairs — the rate
        // source is asked exactly twice, not once per row.
        let fx = CountingFx(AtomicUsize::new(0));
        let mut other_day = row("JPY", 300);
        other_day.date = "2026-09-02".to_string();
        let mut rows = vec![row("JPY", 100), row("JPY", 200), other_day];
        resolve_rendered_spend(&fx, "USD", &mut rows).await;

        assert_eq!(fx.0.load(Ordering::SeqCst), 2, "one lookup per pair");
        // Rate 1.0, JPY (zero-decimal) → USD (2-decimal): ×100 on the way out.
        assert_eq!(
            rows[0].rendered_spend.as_ref().unwrap().amount_minor,
            10_000
        );
        assert_eq!(
            rows[1].rendered_spend.as_ref().unwrap().amount_minor,
            20_000
        );
        assert_eq!(
            rows[2].rendered_spend.as_ref().unwrap().fx_rate_date,
            "2026-09-02"
        );
    }

    // The unused trait imports below are load-bearing only for signatures.
    #[allow(dead_code)]
    fn _witness(_: AdReviewObservation, _: AdReviewState) {}
}
