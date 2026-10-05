//! Handlers: thin actix shims over the application layer.

use std::sync::Arc;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{ApiError, OptionalSubject, RequiredPermission, TenantContext};
use sanvi_kernel::{TenantId, UserId};
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::application::{
    AppealRequest, AppealRequestCommand, ApplyLegalHold, ApplyLegalHoldCommand, DecideAppeal,
    DecideAppealCommand, DownloadExport, DownloadExportCommand, ExtendDeadline,
    ExtendDeadlineCommand, GetDirectives, GetRequestStatus, ListActivities, ListAssessments,
    ListHolds, ListIncidents, ListJurisdictions, ListObligations, ListOpenAppeals,
    ListRequestsOperator, ListRetentionRules, ListSubProcessors, MarkBreachNotified,
    MarkBreachNotifiedCommand, NoticeService, PatchRequest, PublicMetrics, RecordBreachIncident,
    RecordBreachIncidentCommand, RecordConsentChange, RecordConsentChangeCommand,
    RecordLimitSensitive, RecordOptOut, RecordOptOutCommand, RejectRequest, ReleaseLegalHold,
    RemoveSubProcessor, SubmitDsr, SubmitDsrCommand, SubmitTenantRequest, UpsertActivity,
    UpsertActivityCommand, UpsertAssessment, UpsertAssessmentCommand, UpsertJurisdiction,
    UpsertJurisdictionCommand, UpsertRetentionRule, UpsertRetentionRuleCommand, UpsertSubProcessor,
    UpsertSubProcessorCommand, VerifyRequest, VerifyRequestCommand,
};
use crate::domain::{
    AppealId, BreachIncidentId, DirectiveResolver, DsrId, DsrStatus, LegalHoldId, PrivacyError,
    ProcessingPurpose, RectifyPatch, SubjectRef,
};

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct PrivacyState {
    pub submit_dsr: Arc<SubmitDsr>,
    pub submit_tenant_request: Arc<SubmitTenantRequest>,
    pub verify_request: Arc<VerifyRequest>,
    pub get_request_status: Arc<GetRequestStatus>,
    pub list_requests: Arc<ListRequestsOperator>,
    pub extend_deadline: Arc<ExtendDeadline>,
    pub reject_request: Arc<RejectRequest>,
    pub patch_request: Arc<PatchRequest>,
    pub appeal_request: Arc<AppealRequest>,
    pub decide_appeal: Arc<DecideAppeal>,
    pub list_appeals: Arc<ListOpenAppeals>,
    pub get_directives: Arc<GetDirectives>,
    pub record_consent: Arc<RecordConsentChange>,
    pub record_opt_out: Arc<RecordOptOut>,
    pub limit_sensitive: Arc<RecordLimitSensitive>,
    pub download_export: Arc<DownloadExport>,
    pub list_jurisdictions: Arc<ListJurisdictions>,
    pub upsert_jurisdiction: Arc<UpsertJurisdiction>,
    pub apply_hold: Arc<ApplyLegalHold>,
    pub release_hold: Arc<ReleaseLegalHold>,
    pub list_holds: Arc<ListHolds>,
    pub record_incident: Arc<RecordBreachIncident>,
    pub list_incidents: Arc<ListIncidents>,
    pub list_obligations: Arc<ListObligations>,
    pub mark_notified: Arc<MarkBreachNotified>,
    pub list_subprocessors: Arc<ListSubProcessors>,
    pub upsert_subprocessor: Arc<UpsertSubProcessor>,
    pub remove_subprocessor: Arc<RemoveSubProcessor>,
    pub list_activities: Arc<ListActivities>,
    pub upsert_activity: Arc<UpsertActivity>,
    pub list_assessments: Arc<ListAssessments>,
    pub upsert_assessment: Arc<UpsertAssessment>,
    pub list_retention_rules: Arc<ListRetentionRules>,
    pub upsert_retention_rule: Arc<UpsertRetentionRule>,
    pub public_metrics: Arc<PublicMetrics>,
    pub notices: Arc<NoticeService>,
    pub directive_resolver: Arc<dyn DirectiveResolver>,
    pub notice_version: String,
    pub backup_retention_days: i64,
}

/// The privacy context's domain-error → HTTP mapping (single place, this
/// boundary). Careful: privacy errors often carry compliance semantics —
/// "not found" for another subject's request is deliberate (no existence
/// oracle).
pub fn api_error(err: PrivacyError) -> ApiError {
    match err {
        PrivacyError::RequestNotOwned
        | PrivacyError::ArtifactNotFound(_)
        | PrivacyError::ChallengeNotFound
        | PrivacyError::ChallengeMismatch => ApiError::NotFound {
            resource: "request".to_string(),
        },
        PrivacyError::RequestNotFound(name)
        | PrivacyError::SubjectNotFound(name)
        | PrivacyError::AppealNotFound(name)
        | PrivacyError::JurisdictionNotFound(name) => ApiError::NotFound { resource: name },
        PrivacyError::VerificationRequired => ApiError::Unauthorized,
        PrivacyError::ChallengeExpired
        | PrivacyError::ChallengeExhausted
        | PrivacyError::ArtifactExpired
        | PrivacyError::ArtifactDownloadsExhausted
        | PrivacyError::ArtifactPassphraseMismatch => ApiError::Gone {
            detail: err.to_string(),
        },
        PrivacyError::KeyShredded => ApiError::Gone {
            detail: "the subject's data was erased and cannot be recovered".to_string(),
        },
        PrivacyError::AlreadyAppealed
        | PrivacyError::NotAppealable(_)
        | PrivacyError::AppealSelfReview
        | PrivacyError::LegalHoldActive(_)
        | PrivacyError::EssentialNotConsentable
        | PrivacyError::Conflict(_)
        | PrivacyError::MinorSubject(_) => ApiError::Conflict {
            detail: err.to_string(),
        },
        PrivacyError::AgentEvidenceMissing
        | PrivacyError::AgentConsumerNotVerified
        | PrivacyError::SensitiveRequiresConsent => ApiError::BadRequest {
            detail: err.to_string(),
        },
        PrivacyError::EncryptionUnconfigured => ApiError::ServiceUnavailable {
            detail: "privacy processing is temporarily unavailable".to_string(),
        },
        PrivacyError::RateLimited(_) => ApiError::TooManyRequests {
            retry_after_secs: 60,
        },
        PrivacyError::Storage(detail) => {
            tracing::error!(error = %detail, "privacy storage failure");
            ApiError::from_storage_detail(&detail)
        }
        PrivacyError::EncryptionFailure(_)
        | PrivacyError::PackagingFailed(_)
        | PrivacyError::Upstream(_) => {
            tracing::error!(error = %err, "privacy processing failure");
            ApiError::internal("privacy processing failed")
        }
        other => ApiError::BadRequest {
            detail: other.to_string(),
        },
    }
}

/// The current user, or 401 — used by the authenticated self-service routes.
fn require_subject(subject: OptionalSubject) -> Result<sanvi_http::Subject, ApiError> {
    subject.0.ok_or(ApiError::Unauthorized)
}

fn require_actor(subject: OptionalSubject) -> Result<UserId, ApiError> {
    require_subject(subject).map(|subject| subject.user_id)
}

/// Parse a DSR id path segment.
fn dsr_id(path: &web::Path<String>) -> Result<DsrId, ApiError> {
    path.parse::<DsrId>()
        .map_err(|_| ApiError::bad_request("invalid request id"))
}

/// Parse an appeal id path segment.
fn appeal_id(path: &web::Path<String>) -> Result<AppealId, ApiError> {
    path.parse::<AppealId>()
        .map_err(|_| ApiError::bad_request("invalid appeal id"))
}

/// Parse a legal hold id path segment.
fn hold_id(path: &web::Path<String>) -> Result<LegalHoldId, ApiError> {
    path.parse::<LegalHoldId>()
        .map_err(|_| ApiError::bad_request("invalid hold id"))
}

/// Parse a breach incident id path segment.
fn incident_id(path: &web::Path<String>) -> Result<BreachIncidentId, ApiError> {
    path.parse::<BreachIncidentId>()
        .map_err(|_| ApiError::bad_request("invalid incident id"))
}

// ---------------------------------------------------------------------------
// Self-service (subject-scoped)
// ---------------------------------------------------------------------------

/// `POST /api/v1/privacy/requests` — self-service DSR. Authenticated users
/// are verified by their session; email requesters receive an OTP.
#[utoipa::path(
    post,
    path = "/api/v1/privacy/requests",
    tag = "privacy",
    operation_id = "submit_dsr",
    request_body = SubmitDsrCommand,
    responses(
        (status = 201, description = "Request accepted; clocks and jurisdiction resolved", body = crate::application::SubmitDsrOutput),
        (status = 409, description = "Conflict", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn submit_dsr(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<SubmitDsrCommand>,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let ip = req
        .connection_info()
        .realip_remote_addr()
        .and_then(|ip| ip.parse().ok());
    let user = subject.0.as_ref().map(|subject| subject.user_id);
    let output = state
        .submit_dsr
        .execute(user, None, ip, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(output))
}

#[derive(Clone, Deserialize, ToSchema)]
pub struct VerifyRequestRequest {
    pub token: String,
    pub code: String,
}

/// `POST /api/v1/privacy/requests/{id}/verify` — consume the OTP.
#[utoipa::path(
    post,
    path = "/api/v1/privacy/requests/{id}/verify",
    tag = "privacy",
    operation_id = "verify_dsr",
    params(("id" = String, Path, description = "The request id")),
    request_body = VerifyRequestRequest,
    responses(
        (status = 200, description = "Verified", body = crate::application::VerifyRequestOutput),
        (status = 404, description = "Challenge not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn verify_dsr(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    body: web::Json<VerifyRequestRequest>,
) -> Result<impl Responder, ApiError> {
    let output = state
        .verify_request
        .execute(VerifyRequestCommand {
            request_id: dsr_id(&path)?,
            token: body.token.clone(),
            code: body.code.clone(),
        })
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(output))
}

#[derive(Clone, Deserialize, ToSchema)]
pub struct GetRequestQuery {
    pub token: Option<String>,
}

/// `GET /api/v1/privacy/requests/{id}` — status for the subject (owned by
/// session or challenge token).
#[utoipa::path(
    get,
    path = "/api/v1/privacy/requests/{id}",
    tag = "privacy",
    operation_id = "get_dsr_status",
    params(("id" = String, Path, description = "The request id"), ("token" = Option<String>, Query, description = "Challenge token (for unauthenticated requesters)")),
    responses(
        (status = 200, description = "Status incl. ack time and due date", body = crate::application::GetRequestStatusOutput),
        (status = 404, description = "Unknown request", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_dsr_status(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    query: web::Query<GetRequestQuery>,
    subject: OptionalSubject,
) -> Result<impl Responder, ApiError> {
    let user = subject.0.map(|subject| subject.user_id);
    let output = state
        .get_request_status
        .execute(dsr_id(&path)?, user, query.token.as_deref())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(output))
}

#[derive(Clone, Deserialize, ToSchema)]
pub struct DownloadQuery {
    pub token: String,
}

#[derive(Clone, Deserialize, ToSchema)]
pub struct DownloadHeaders {
    #[serde(rename = "x-export-passphrase")]
    pub passphrase: String,
}

/// `GET /api/v1/privacy/requests/{id}/download` — one-time, TTL-bound export
/// download. The passphrase was delivered out of band; the token is the
/// download link's secret.
#[utoipa::path(
    get,
    path = "/api/v1/privacy/requests/{id}/download",
    tag = "privacy",
    operation_id = "download_export",
    params(("id" = String, Path, description = "The request id"), ("token" = String, Query, description = "Download token")),
    responses(
        (status = 200, description = "The encrypted export archive", body = Vec<u8>),
        (status = 404, description = "Unknown artifact", body = sanvi_http::ProblemDetail),
        (status = 410, description = "Expired or exhausted", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn download_export(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    query: web::Query<DownloadQuery>,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let passphrase = req
        .headers()
        .get("x-export-passphrase")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let output = state
        .download_export
        .execute(DownloadExportCommand {
            request_id: dsr_id(&path)?,
            token: query.token.clone(),
            passphrase: passphrase.to_string(),
        })
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok()
        .insert_header((
            "Content-Disposition",
            format!("attachment; filename=\"export-{}.enc\"", output.artifact_id),
        ))
        .insert_header(("X-Export-Sha256", output.manifest_sha256.clone()))
        .body(output.content))
}

/// `POST /api/v1/privacy/opt-out` — sale/share, targeted advertising,
/// profiling. NO verification gate; accepts `Sec-GPC`.
#[utoipa::path(
    post,
    path = "/api/v1/privacy/opt-out",
    tag = "privacy",
    operation_id = "record_opt_out",
    request_body = RecordOptOutCommand,
    responses(
        (status = 200, description = "Directives flipped immediately", body = crate::application::RecordOptOutOutput)
    )
)]
pub async fn record_opt_out(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<RecordOptOutCommand>,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let mut command = body.into_inner();
    if command.source == "gpc" {
        let gpc = req
            .headers()
            .get("Sec-GPC")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim() == "1");
        if !gpc {
            command.source = "ui".to_string();
        }
    }
    let subject_ref = match &subject.0 {
        Some(user) => SubjectRef::user(user.user_id),
        None => {
            // Pre-authentication: the device cookie identifies the subject.
            let device = req
                .headers()
                .get(actix_web::http::header::COOKIE)
                .and_then(|value| value.to_str().ok())
                .and_then(|header| {
                    header.split(';').find_map(|pair| {
                        let mut parts = pair.splitn(2, '=');
                        let name = parts.next()?.trim();
                        let value = parts.next().unwrap_or("").trim();
                        (name == "sanvi_gpc_device" && !value.is_empty()).then(|| value.to_string())
                    })
                })
                .unwrap_or_else(|| Uuid::now_v7().to_string());
            SubjectRef::device(&device)
        }
    };
    let output = state
        .record_opt_out
        .execute(&subject_ref, command)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(output))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LimitSensitiveRequest {
    pub source: Option<String>,
}

/// `POST /api/v1/privacy/limit-sensitive` — CPRA "limit the use of my
/// sensitive personal information".
#[utoipa::path(
    post,
    path = "/api/v1/privacy/limit-sensitive",
    tag = "privacy",
    operation_id = "limit_sensitive_use",
    request_body = LimitSensitiveRequest,
    responses(
        (status = 200, description = "Sensitive use limited", body = crate::application::RecordOptOutOutput)
    )
)]
pub async fn limit_sensitive_use(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<LimitSensitiveRequest>,
) -> Result<impl Responder, ApiError> {
    let subject_ref = match &subject.0 {
        Some(user) => SubjectRef::user(user.user_id),
        None => return Err(ApiError::Unauthorized),
    };
    let output = state
        .limit_sensitive
        .execute(
            &subject_ref,
            body.source.clone().unwrap_or_else(|| "api".to_string()),
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(output))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ConsentChangeRequest {
    pub purpose: ProcessingPurpose,
    pub granted: bool,
    pub notice_version: String,
    pub method: String,
    pub locale: Option<String>,
    pub signal_source: Option<String>,
}

/// `PUT /api/v1/privacy/consents` — grant/withdraw one consent.
#[utoipa::path(
    put,
    path = "/api/v1/privacy/consents",
    tag = "privacy",
    operation_id = "update_consents",
    request_body = ConsentChangeRequest,
    responses(
        (status = 200, description = "Recorded and materialised", body = crate::application::RecordConsentChangeOutput),
        (status = 409, description = "Essential is not consentable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn update_consents(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<ConsentChangeRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let user = require_subject(subject)?;
    let command = body.into_inner();
    let output = state
        .record_consent
        .execute(
            &SubjectRef::user(user.user_id),
            RecordConsentChangeCommand {
                purpose: command.purpose,
                granted: command.granted,
                notice_version: command.notice_version,
                method: command.method,
                locale: command.locale,
                signal_source: command.signal_source,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(output))
}

/// `GET /api/v1/privacy/consents` — own consent ledger (proof view).
#[utoipa::path(
    get,
    path = "/api/v1/privacy/consents",
    tag = "privacy",
    operation_id = "get_consents",
    responses(
        (status = 200, description = "Consent ledger rows for the subject", body = Vec<crate::domain::ConsentRecord>)
    )
)]
pub async fn get_consents(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let user = require_subject(subject)?;
    let snapshot = state
        .directive_resolver
        .snapshot(&SubjectRef::user(user.user_id))
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(snapshot))
}

/// `GET /api/v1/privacy/directives` — the resolved state per purpose, with
/// source and jurisdiction (what the privacy centre renders). Binding
/// unbound GPC signals happens here.
#[utoipa::path(
    get,
    path = "/api/v1/privacy/directives",
    tag = "privacy",
    operation_id = "get_directives",
    responses(
        (status = 200, description = "Resolved directives", body = crate::application::GetDirectivesOutput)
    )
)]
pub async fn get_directives(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let user = require_subject(subject)?;
    let output = state
        .get_directives
        .execute(&SubjectRef::user(user.user_id))
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(output))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AppealRequestRequest {
    pub reason: String,
}

/// `POST /api/v1/privacy/requests/{id}/appeal` — appeal a refusal (US).
#[utoipa::path(
    post,
    path = "/api/v1/privacy/requests/{id}/appeal",
    tag = "privacy",
    operation_id = "appeal_request",
    params(("id" = String, Path, description = "The refused request id")),
    request_body = AppealRequestRequest,
    responses(
        (status = 201, description = "Appeal accepted; authority contact included", body = crate::application::AppealRequestOutput),
        (status = 409, description = "Not appealable", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn appeal_request(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    body: web::Json<AppealRequestRequest>,
) -> Result<impl Responder, ApiError> {
    let user = require_subject(subject)?;
    let output = state
        .appeal_request
        .execute(
            Some(user.user_id),
            AppealRequestCommand {
                request_id: dsr_id(&path)?,
                reason: body.reason.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(output))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RectifyRequest {
    pub corrections: Vec<crate::domain::Correction>,
}

/// `PATCH /api/v1/privacy/requests/{id}/rectify` — apply corrections.
#[utoipa::path(
    patch,
    path = "/api/v1/privacy/requests/{id}/rectify",
    tag = "privacy",
    operation_id = "rectify_request",
    params(("id" = String, Path, description = "The rectification request id")),
    request_body = RectifyRequest,
    responses((status = 200, description = "Corrections applied"))
)]
pub async fn rectify_request(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    body: web::Json<RectifyRequest>,
) -> Result<impl Responder, ApiError> {
    let user = require_subject(subject)?;
    state
        .patch_request
        .execute(
            user.user_id,
            dsr_id(&path)?,
            RectifyPatch {
                corrections: body.corrections.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}

// ---------------------------------------------------------------------------
// Public disclosures
// ---------------------------------------------------------------------------

/// `GET /api/v1/public/privacy/notice` — the current notice + version,
/// generated from live configuration (jurisdiction sections and the
/// retention table).
#[utoipa::path(
    get,
    path = "/api/v1/public/privacy/notice",
    tag = "privacy",
    operation_id = "get_privacy_notice",
    responses((status = 200, description = "The privacy notice", body = crate::application::PrivacyNoticeView))
)]
pub async fn get_privacy_notice(
    state: web::Data<PrivacyState>,
) -> Result<impl Responder, ApiError> {
    let notice = state
        .notices
        .build(&state.notice_version, state.backup_retention_days)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(notice))
}

/// `GET /api/v1/public/privacy/notice-at-collection` — categories, purposes,
/// retention and sale/share status, served before any collection.
#[utoipa::path(
    get,
    path = "/api/v1/public/privacy/notice-at-collection",
    tag = "privacy",
    operation_id = "get_notice_at_collection",
    responses((status = 200, description = "Notice at collection", body = crate::application::NoticeAtCollectionView))
)]
pub async fn get_notice_at_collection(
    state: web::Data<PrivacyState>,
) -> Result<impl Responder, ApiError> {
    let rules = state
        .list_retention_rules
        .execute()
        .await
        .map_err(api_error)?;
    let view = crate::application::NoticeAtCollectionView::build(&state.notice_version, &rules);
    Ok(HttpResponse::Ok().json(view))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct MetricsQuery {
    pub year: Option<i32>,
}

/// `GET /api/v1/public/privacy/metrics` — annual request metrics (CCPA
/// disclosure), generated from the request ledger.
#[utoipa::path(
    get,
    path = "/api/v1/public/privacy/metrics",
    tag = "privacy",
    operation_id = "get_public_metrics",
    params(("year" = Option<i32>, Query, description = "Disclosure year (default: previous year)")),
    responses((status = 200, description = "Annual metrics", body = Vec<crate::application::PublicMetricsRow>))
)]
pub async fn get_public_metrics(
    state: web::Data<PrivacyState>,
    query: web::Query<MetricsQuery>,
) -> Result<impl Responder, ApiError> {
    let rows = state
        .public_metrics
        .execute(query.year)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(rows))
}

/// `GET /api/v1/public/subprocessors` — the public sub-processor list with
/// role and transfer mechanism.
#[utoipa::path(
    get,
    path = "/api/v1/public/subprocessors",
    tag = "privacy",
    operation_id = "list_public_subprocessors",
    responses((status = 200, description = "Sub-processors", body = Vec<crate::domain::SubProcessor>))
)]
pub async fn list_public_subprocessors(
    state: web::Data<PrivacyState>,
) -> Result<impl Responder, ApiError> {
    let processors = state
        .list_subprocessors
        .execute(false)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(processors))
}

// ---------------------------------------------------------------------------
// Tenant processor mode
// ---------------------------------------------------------------------------

fn current_tenant(req: &HttpRequest) -> Result<TenantId, ApiError> {
    req.extensions()
        .get::<TenantContext>()
        .map(|context| context.tenant_id)
        .ok_or_else(|| ApiError::NotFound {
            resource: "tenant".to_string(),
        })
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TenantRequestQuery {
    pub status: Option<DsrStatus>,
}

#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PrivacyPageQuery {
    /// Page size, clamped to 1..=200.
    #[param(schema_with = privacy_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last item of the previous page. Absent for the
    /// first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn privacy_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 200).build()
}

fn privacy_cursor(after: &Option<String>) -> Result<Option<i64>, ApiError> {
    after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))
}

/// `POST /api/v1/tenant/privacy/requests` — tenant admin acting for their
/// end users (processor / service-provider role).
#[utoipa::path(
    post,
    path = "/api/v1/tenant/privacy/requests",
    tag = "privacy",
    operation_id = "submit_tenant_dsr",
    request_body = SubmitDsrCommand,
    responses(
        (status = 201, description = "Request accepted", body = crate::application::SubmitDsrOutput),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn submit_tenant_dsr(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<SubmitDsrCommand>,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let user = require_subject(subject)?;
    let tenant_id = current_tenant(&req)?;
    let ip = req
        .connection_info()
        .realip_remote_addr()
        .and_then(|ip| ip.parse().ok());
    let output = state
        .submit_tenant_request
        .execute(user.user_id, tenant_id, ip, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(output))
}

/// `GET /api/v1/tenant/privacy/requests` — the tenant's own requests.
#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TenantRequestPageQuery {
    /// Filter by status.
    pub status: Option<DsrStatus>,
    /// Page size, clamped to 1..=200.
    #[param(schema_with = privacy_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last request of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    pub after: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/tenant/privacy/requests",
    tag = "privacy",
    operation_id = "list_tenant_dsrs",
    params(TenantRequestPageQuery),
    responses(
        (status = 200, description = "Tenant's requests", body = Vec<crate::application::RequestOperatorView>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_tenant_dsrs(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
    query: web::Query<TenantRequestPageQuery>,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let tenant_id = current_tenant(&req)?;
    let after_id = privacy_cursor(&query.after)?;
    let requests = state
        .list_requests
        .execute_for_tenant(tenant_id, query.status, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(requests))
}

// ---------------------------------------------------------------------------
// Platform operator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListRequestsQuery {
    /// Filter by status.
    pub status: Option<DsrStatus>,
    /// Page size, clamped to 1..=200.
    #[param(schema_with = privacy_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last request of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    pub after: Option<String>,
}

/// `GET /api/v1/platform/privacy/requests` — operator view.
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/requests",
    tag = "privacy",
    operation_id = "list_dsrs",
    params(ListRequestsQuery),
    responses(
        (status = 200, description = "Requests", body = Vec<crate::application::RequestOperatorView>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_dsrs(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
    query: web::Query<ListRequestsQuery>,
) -> Result<impl Responder, ApiError> {
    let after_id = privacy_cursor(&query.after)?;
    let requests = state
        .list_requests
        .execute(query.status, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(requests))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RejectRequestRequest {
    pub reason: String,
}

/// `POST /api/v1/platform/privacy/requests/{id}/reject` — reject with
/// reason (US subjects get the appeal route in the response).
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/requests/{id}/reject",
    tag = "privacy",
    operation_id = "reject_dsr",
    params(("id" = String, Path, description = "The request id")),
    request_body = RejectRequestRequest,
    responses((status = 200, description = "Rejected"))
)]
pub async fn reject_dsr(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    body: web::Json<RejectRequestRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    state
        .reject_request
        .execute(actor, dsr_id(&path)?, body.reason.clone())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}

/// `POST /api/v1/platform/privacy/requests/{id}/extend` — one deadline
/// extension (jurisdiction rules apply).
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/requests/{id}/extend",
    tag = "privacy",
    operation_id = "extend_dsr",
    params(("id" = String, Path, description = "The request id")),
    responses((status = 200, description = "New deadline", body = chrono::DateTime<chrono::Utc>))
)]
pub async fn extend_dsr(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    let deadline = state
        .extend_deadline
        .execute(
            actor,
            ExtendDeadlineCommand {
                request_id: dsr_id(&path)?,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(deadline))
}

/// `GET /api/v1/platform/privacy/appeals` — open appeals.
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/appeals",
    tag = "privacy",
    operation_id = "list_appeals",
    params(PrivacyPageQuery),
    responses(
        (status = 200, description = "One page of open appeals, oldest first", body = Vec<crate::domain::Appeal>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_appeals(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
    query: web::Query<PrivacyPageQuery>,
) -> Result<impl Responder, ApiError> {
    let after_id = privacy_cursor(&query.after)?;
    let appeals = state
        .list_appeals
        .execute(query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(appeals))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DecideAppealRequest {
    pub outcome: crate::domain::AppealOutcome,
    pub reason: String,
}

/// `POST /api/v1/platform/privacy/appeals/{id}/decision` — second-operator
/// review.
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/appeals/{id}/decision",
    tag = "privacy",
    operation_id = "decide_appeal",
    params(("id" = String, Path, description = "The appeal id")),
    request_body = DecideAppealRequest,
    responses(
        (status = 200, description = "Decision recorded", body = crate::application::DecideAppealOutput),
        (status = 409, description = "Self-review refused", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn decide_appeal(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    body: web::Json<DecideAppealRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    let output = state
        .decide_appeal
        .execute(
            actor,
            DecideAppealCommand {
                appeal_id: appeal_id(&path)?,
                outcome: body.outcome,
                reason: body.reason.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(output))
}

/// `GET /api/v1/platform/privacy/jurisdictions` — profiles.
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/jurisdictions",
    tag = "privacy",
    operation_id = "list_jurisdictions",
    responses((status = 200, description = "Jurisdiction profiles", body = Vec<crate::domain::JurisdictionProfile>))
)]
pub async fn list_jurisdictions(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let profiles = state
        .list_jurisdictions
        .execute()
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(profiles))
}

/// `PUT /api/v1/platform/privacy/jurisdictions` — upsert a profile (audited
/// configuration; a new state law is this call plus a matrix-test row).
#[utoipa::path(
    put,
    path = "/api/v1/platform/privacy/jurisdictions",
    tag = "privacy",
    operation_id = "upsert_jurisdiction",
    request_body = UpsertJurisdictionCommand,
    responses((status = 200, description = "Profile saved"))
)]
pub async fn upsert_jurisdiction(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<UpsertJurisdictionCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    state
        .upsert_jurisdiction
        .execute(actor, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}

/// `GET /api/v1/platform/privacy/holds` — legal holds.
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/holds",
    tag = "privacy",
    operation_id = "list_holds",
    params(PrivacyPageQuery),
    responses(
        (status = 200, description = "One page of legal holds, newest first", body = Vec<crate::domain::LegalHold>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_holds(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
    query: web::Query<PrivacyPageQuery>,
) -> Result<impl Responder, ApiError> {
    let after_id = privacy_cursor(&query.after)?;
    let holds = state
        .list_holds
        .execute(true, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(holds))
}

/// `POST /api/v1/platform/privacy/holds` — apply a legal hold (blocks
/// erasure; audited).
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/holds",
    tag = "privacy",
    operation_id = "apply_hold",
    request_body = ApplyLegalHoldCommand,
    responses((status = 201, description = "Hold applied", body = String))
)]
pub async fn apply_hold(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<ApplyLegalHoldCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    let hold_id = state
        .apply_hold
        .execute(actor, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(hold_id))
}

/// `POST /api/v1/platform/privacy/holds/{id}/release` — release a hold.
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/holds/{id}/release",
    tag = "privacy",
    operation_id = "release_hold",
    params(("id" = String, Path, description = "The hold id")),
    responses((status = 200, description = "Hold released"))
)]
pub async fn release_hold(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    state
        .release_hold
        .execute(actor, hold_id(&path)?)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}

/// `GET /api/v1/platform/privacy/incidents` — breach incidents.
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/incidents",
    tag = "privacy",
    operation_id = "list_incidents",
    params(PrivacyPageQuery),
    responses(
        (status = 200, description = "One page of incidents, newest first", body = Vec<crate::domain::BreachIncident>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_incidents(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
    query: web::Query<PrivacyPageQuery>,
) -> Result<impl Responder, ApiError> {
    let after_id = privacy_cursor(&query.after)?;
    let incidents = state
        .list_incidents
        .execute(query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(incidents))
}

/// `POST /api/v1/platform/privacy/incidents` — record an incident; the
/// response includes the computed per-jurisdiction obligations.
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/incidents",
    tag = "privacy",
    operation_id = "record_incident",
    request_body = RecordBreachIncidentCommand,
    responses(
        (status = 201, description = "Incident + obligations", body = crate::application::RecordBreachIncidentOutput)
    )
)]
pub async fn record_incident(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<RecordBreachIncidentCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    let output = state
        .record_incident
        .execute(actor, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(output))
}

/// `GET /api/v1/platform/privacy/incidents/{id}/obligations` — computed
/// obligations.
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/incidents/{id}/obligations",
    tag = "privacy",
    operation_id = "list_obligations",
    params(
        ("id" = String, Path, description = "The incident id"),
        PrivacyPageQuery
    ),
    responses(
        (status = 200, description = "One page of the incident's obligations, newest first", body = Vec<crate::domain::NotificationObligation>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_obligations(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
    query: web::Query<PrivacyPageQuery>,
) -> Result<impl Responder, ApiError> {
    let after_id = privacy_cursor(&query.after)?;
    let obligations = state
        .list_obligations
        .execute(incident_id(&path)?, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(obligations))
}

/// `POST /api/v1/platform/privacy/incidents/{id}/notify` — record a
/// discharged obligation.
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/incidents/{id}/notify",
    tag = "privacy",
    operation_id = "mark_obligation_notified",
    params(("id" = String, Path, description = "The incident id")),
    request_body = MarkBreachNotifiedCommand,
    responses((status = 200, description = "Recorded"))
)]
pub async fn mark_obligation_notified(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    body: web::Json<MarkBreachNotifiedCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    state
        .mark_notified
        .execute(actor, incident_id(&path)?, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}

/// `GET /api/v1/platform/privacy/subprocessors` — registry (incl. removed).
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/subprocessors",
    tag = "privacy",
    operation_id = "list_subprocessors",
    responses((status = 200, description = "Sub-processors", body = Vec<crate::domain::SubProcessor>))
)]
pub async fn list_subprocessors(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let processors = state
        .list_subprocessors
        .execute(true)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(processors))
}

/// `POST /api/v1/platform/privacy/subprocessors` — upsert (role is
/// required: it decides what is a "sale" under US law).
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/subprocessors",
    tag = "privacy",
    operation_id = "upsert_subprocessor",
    request_body = UpsertSubProcessorCommand,
    responses((status = 200, description = "Saved"))
)]
pub async fn upsert_subprocessor(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<UpsertSubProcessorCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    state
        .upsert_subprocessor
        .execute(actor, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}

/// `DELETE /api/v1/platform/privacy/subprocessors/{name}` — remove.
#[utoipa::path(
    delete,
    path = "/api/v1/platform/privacy/subprocessors/{name}",
    tag = "privacy",
    operation_id = "remove_subprocessor",
    params(("name" = String, Path, description = "The sub-processor name")),
    responses((status = 200, description = "Removed"))
)]
pub async fn remove_subprocessor(
    state: web::Data<PrivacyState>,
    path: web::Path<String>,
    subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    state
        .remove_subprocessor
        .execute(actor, &path.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}

/// `GET /api/v1/platform/privacy/activities` — Art. 30 records.
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/activities",
    tag = "privacy",
    operation_id = "list_activities",
    responses((status = 200, description = "Processing activities", body = Vec<crate::domain::ProcessingActivity>))
)]
pub async fn list_activities(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let activities = state.list_activities.execute().await.map_err(api_error)?;
    Ok(HttpResponse::Ok().json(activities))
}

/// `POST /api/v1/platform/privacy/activities` — record a processing
/// activity.
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/activities",
    tag = "privacy",
    operation_id = "upsert_activity",
    request_body = UpsertActivityCommand,
    responses((status = 201, description = "Recorded", body = String))
)]
pub async fn upsert_activity(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<UpsertActivityCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    let id = state
        .upsert_activity
        .execute(actor, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(id))
}

/// `GET /api/v1/platform/privacy/assessments` — risk assessments (DPIA +
/// US state data-protection assessments).
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/assessments",
    tag = "privacy",
    operation_id = "list_assessments",
    params(PrivacyPageQuery),
    responses(
        (status = 200, description = "One page of risk assessments, newest first", body = Vec<crate::domain::RiskAssessment>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_assessments(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
    query: web::Query<PrivacyPageQuery>,
) -> Result<impl Responder, ApiError> {
    let after_id = privacy_cursor(&query.after)?;
    let assessments = state
        .list_assessments
        .execute(query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(assessments))
}

/// `POST /api/v1/platform/privacy/assessments` — record an assessment.
#[utoipa::path(
    post,
    path = "/api/v1/platform/privacy/assessments",
    tag = "privacy",
    operation_id = "upsert_assessment",
    request_body = UpsertAssessmentCommand,
    responses((status = 201, description = "Recorded", body = String))
)]
pub async fn upsert_assessment(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<UpsertAssessmentCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    let id = state
        .upsert_assessment
        .execute(actor, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(id))
}

/// `GET /api/v1/platform/privacy/retention-rules` — retention rules (the
/// notice's retention table comes from these rows).
#[utoipa::path(
    get,
    path = "/api/v1/platform/privacy/retention-rules",
    tag = "privacy",
    operation_id = "list_retention_rules",
    responses((status = 200, description = "Retention rules", body = Vec<crate::domain::RetentionRule>))
)]
pub async fn list_retention_rules(
    state: web::Data<PrivacyState>,
    _subject: OptionalSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let rules = state
        .list_retention_rules
        .execute()
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(rules))
}

/// `PUT /api/v1/platform/privacy/retention-rules` — adjust a rule (counsel
/// changes periods without a release).
#[utoipa::path(
    put,
    path = "/api/v1/platform/privacy/retention-rules",
    tag = "privacy",
    operation_id = "upsert_retention_rule",
    request_body = UpsertRetentionRuleCommand,
    responses((status = 200, description = "Saved"))
)]
pub async fn upsert_retention_rule(
    state: web::Data<PrivacyState>,
    subject: OptionalSubject,
    body: web::Json<UpsertRetentionRuleCommand>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let actor = require_actor(subject)?;
    state
        .upsert_retention_rule
        .execute(actor, body.into_inner())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().finish())
}
