//! Handlers: thin actix shims over the application layer.

use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, Responder, web};
use chrono::{DateTime, Utc};
use sanvi_http::{ApiError, CurrentSubject, FreshAal2Required, RequiredPermission};
use sanvi_kernel::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::{
    ApproveRequest, CreateImpersonation, CreateImpersonationCommand, DecideApprovalCommand,
    GetTenantAdminView, ImpersonationGrantView, ListImpersonations, ListPendingApprovals,
    RejectRequest, RevokeImpersonation, SearchTenantAdminViews,
};
use crate::domain::PlatformAdminError;
use crate::domain::approvals::ApprovalId;
use crate::domain::impersonation::{ImpersonationGrantId, ImpersonationMode};
use crate::domain::ports::{EntitlementAdmin, GrantOverrideOutcome};
use crate::domain::views::{TenantAdminView, TenantViewCursor, TenantViewSearch};

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct PlatformAdminState {
    pub search_tenants: Arc<SearchTenantAdminViews>,
    pub get_tenant: Arc<GetTenantAdminView>,
    pub create_impersonation: Arc<CreateImpersonation>,
    pub list_impersonations: Arc<ListImpersonations>,
    pub revoke_impersonation: Arc<RevokeImpersonation>,
    pub entitlements: Arc<dyn EntitlementAdmin>,
    pub list_approvals: Arc<ListPendingApprovals>,
    pub approve: Arc<ApproveRequest>,
    pub reject: Arc<RejectRequest>,
    /// 09.6: the platform's per-tenant application-fee administration.
    /// Lives here, never in the tenant API — a tenant cannot set the
    /// platform's own fee.
    pub fees: Option<Arc<dyn crate::domain::ports::ApplicationFeeAdmin>>,
}

/// The platform_admin context's domain-error → HTTP mapping (single place,
/// this boundary).
pub fn api_error(err: PlatformAdminError) -> ApiError {
    match err {
        PlatformAdminError::MissingReason
        | PlatformAdminError::InvalidDuration
        | PlatformAdminError::DurationTooLong { .. }
        | PlatformAdminError::InvalidActionType(_)
        | PlatformAdminError::InvalidSearch(_)
        | PlatformAdminError::InvalidEntitlement(_)
        | PlatformAdminError::InvalidFeeConfig(_) => ApiError::BadRequest {
            detail: err.to_string(),
        },
        PlatformAdminError::FeatureNotFound(name) => ApiError::NotFound {
            resource: format!("feature {name}"),
        },
        PlatformAdminError::GrantNotFound { .. }
        | PlatformAdminError::ApprovalNotFound { .. }
        | PlatformAdminError::TenantNotFound { .. } => ApiError::NotFound {
            resource: err.to_string(),
        },
        PlatformAdminError::GrantRevoked { .. } | PlatformAdminError::GrantExpired { .. } => {
            ApiError::Gone {
                detail: err.to_string(),
            }
        }
        PlatformAdminError::TargetNotMember { .. }
        | PlatformAdminError::PrivacyLocked { .. }
        | PlatformAdminError::SelfApproval => ApiError::Forbidden {
            detail: err.to_string(),
        },
        PlatformAdminError::ApprovalNotPending { .. }
        | PlatformAdminError::EntitlementConflict(_) => ApiError::Conflict {
            detail: err.to_string(),
        },
        PlatformAdminError::Storage(detail) => {
            tracing::error!(error = %detail, "platform_admin storage failure");
            ApiError::from_storage_detail(&detail)
        }
        PlatformAdminError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "platform_admin ID generation failure");
            ApiError::ServiceUnavailable {
                detail: "ID generation is temporarily unavailable".to_string(),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tenant admin views
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TenantSearchQuery {
    /// Substring match on slug/display name.
    pub query: Option<String>,
    /// Tenant status filter.
    pub status: Option<String>,
    /// Cursor: created_at of the last row.
    pub after_created_at: Option<DateTime<Utc>>,
    /// Cursor: tenant id of the last row.
    #[param(value_type = Option<String>)]
    pub after_id: Option<TenantId>,
    /// Page size, clamped to 1..=100.
    #[param(schema_with = admin_limit_schema)]
    pub limit: Option<u32>,
}

fn admin_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(100, 25).build()
}

/// One page of the operator tenant search.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TenantAdminPageView {
    pub tenants: Vec<TenantAdminView>,
    pub next_cursor: Option<TenantViewCursorView>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TenantViewCursorView {
    pub created_at: DateTime<Utc>,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
}

/// `GET /api/v1/platform/admin/tenants` — operator tenant search.
#[utoipa::path(
    get,
    path = "/api/v1/platform/admin/tenants",
    tag = "platform_admin",
    operation_id = "search_tenant_admin_views",
    params(TenantSearchQuery),
    responses(
        (status = 200, description = "One page of tenant views", body = TenantAdminPageView),
        (status = 400, description = "Invalid filter", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn search_tenant_admin_views(
    state: web::Data<PlatformAdminState>,
    query: web::Query<TenantSearchQuery>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let after = match (query.after_created_at, query.after_id) {
        (Some(created_at), Some(tenant_id)) => Some(TenantViewCursor {
            created_at,
            tenant_id,
        }),
        (None, None) => None,
        _ => {
            return Err(ApiError::BadRequest {
                detail: "after_created_at and after_id must be provided together".to_string(),
            });
        }
    };
    let (tenants, next_cursor) = state
        .search_tenants
        .execute(&TenantViewSearch {
            query: query.query.clone(),
            status: query.status.clone(),
            after,
            limit: query.limit,
        })
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(TenantAdminPageView {
        tenants,
        next_cursor: next_cursor.map(|cursor| TenantViewCursorView {
            created_at: cursor.created_at,
            tenant_id: cursor.tenant_id,
        }),
    }))
}

/// `GET /api/v1/platform/admin/tenants/{id}` — one tenant admin view.
#[utoipa::path(
    get,
    path = "/api/v1/platform/admin/tenants/{id}",
    tag = "platform_admin",
    operation_id = "get_tenant_admin_view",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 200, description = "The tenant view", body = TenantAdminView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Tenant not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_tenant_admin_view(
    state: web::Data<PlatformAdminState>,
    path: web::Path<TenantId>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let view = state
        .get_tenant
        .execute(path.into_inner())
        .await
        .map_err(api_error)?
        .ok_or_else(|| ApiError::NotFound {
            resource: "tenant".to_string(),
        })?;
    Ok(HttpResponse::Ok().json(view))
}

// ---------------------------------------------------------------------------
// Impersonation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateImpersonationRequest {
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    #[schema(value_type = String)]
    pub target_user_id: UserId,
    pub reason: String,
    #[serde(default)]
    pub mode: ImpersonationMode,
    pub duration_minutes: u32,
}

/// `POST /api/v1/platform/impersonations` — create a time-boxed grant
/// (requires fresh MFA). Exchange the returned id via the
/// `x-sanvi-impersonate` header.
#[utoipa::path(
    post,
    path = "/api/v1/platform/impersonations",
    tag = "platform_admin",
    operation_id = "create_impersonation",
    request_body = CreateImpersonationRequest,
    responses(
        (status = 201, description = "Grant created", body = ImpersonationGrantView),
        (status = 400, description = "Invalid request", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied, target not a member, or fresh MFA missing", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_impersonation(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    body: web::Json<CreateImpersonationRequest>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let grant = state
        .create_impersonation
        .execute(
            subject.0.user_id,
            sanvi_http::impersonation_actor(&req),
            CreateImpersonationCommand {
                tenant_id: body.tenant_id,
                target_user_id: body.target_user_id,
                reason: body.reason.clone(),
                mode: body.mode,
                duration_minutes: body.duration_minutes,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(grant))
}

#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListImpersonationsQuery {
    /// Restrict to one tenant.
    #[param(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    /// Page size, clamped to 1..=100.
    #[param(schema_with = impersonation_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last grant of the previous page. Absent for the
    /// first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn impersonation_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(100, 100).build()
}

/// `GET /api/v1/platform/impersonations` — active grants.
#[utoipa::path(
    get,
    path = "/api/v1/platform/impersonations",
    tag = "platform_admin",
    operation_id = "list_impersonations",
    params(ListImpersonationsQuery),
    responses(
        (status = 200, description = "Active grants", body = Vec<ImpersonationGrantView>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_impersonations(
    state: web::Data<PlatformAdminState>,
    query: web::Query<ListImpersonationsQuery>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let grants = state
        .list_impersonations
        .execute(query.tenant_id, query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(grants))
}

/// `DELETE /api/v1/platform/impersonations/{id}` — revoke (immediate).
#[utoipa::path(
    delete,
    path = "/api/v1/platform/impersonations/{id}",
    tag = "platform_admin",
    operation_id = "revoke_impersonation",
    params(("id" = String, Path, description = "Grant id")),
    responses(
        (status = 200, description = "Revoked grant", body = ImpersonationGrantView),
        (status = 403, description = "Permission denied or fresh MFA missing", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Grant not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn revoke_impersonation(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    path: web::Path<ImpersonationGrantId>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let grant = state
        .revoke_impersonation
        .execute(
            subject.0.user_id,
            sanvi_http::impersonation_actor(&req),
            path.into_inner(),
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(grant))
}

// ---------------------------------------------------------------------------
// Entitlement overrides (operator console)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct GrantEntitlementRequest {
    pub feature_key: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub limit: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub reason: Option<String>,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrantEntitlementResultView {
    Granted,
    ApprovalRequired {
        #[schema(value_type = String)]
        approval_id: ApprovalId,
    },
}

/// `POST /api/v1/platform/tenants/{id}/entitlements` — grant an override.
/// Overrides above the configured threshold require four-eyes approval and
/// return `approval_required` with the pending request id.
#[utoipa::path(
    post,
    path = "/api/v1/platform/tenants/{id}/entitlements",
    tag = "platform_admin",
    operation_id = "grant_entitlement_override",
    params(("id" = String, Path, description = "Tenant id")),
    request_body = GrantEntitlementRequest,
    responses(
        (status = 201, description = "Granted (or approval required)", body = GrantEntitlementResultView),
        (status = 400, description = "Invalid request", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied or fresh MFA missing", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn grant_entitlement_override(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    path: web::Path<TenantId>,
    body: web::Json<GrantEntitlementRequest>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<impl Responder, ApiError> {
    let outcome = state
        .entitlements
        .grant_override(
            subject.0.user_id,
            path.into_inner(),
            body.feature_key.clone(),
            body.enabled,
            body.limit,
            body.expires_at,
            body.reason.clone(),
        )
        .await
        .map_err(api_error)?;
    match outcome {
        GrantOverrideOutcome::Granted => {
            Ok(HttpResponse::Created().json(GrantEntitlementResultView::Granted))
        }
        GrantOverrideOutcome::ApprovalRequired { approval_id } => Ok(HttpResponse::Accepted()
            .json(GrantEntitlementResultView::ApprovalRequired { approval_id })),
    }
}

/// One stored override row (operator console).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OverrideView {
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    pub feature: String,
    pub enabled: bool,
    pub limit: Option<i64>,
    pub expires_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub granted_by: Option<UserId>,
    pub granted_reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// `GET /api/v1/platform/tenants/{id}/entitlements` — a tenant's stored
/// override rows.
#[utoipa::path(
    get,
    path = "/api/v1/platform/tenants/{id}/entitlements",
    tag = "platform_admin",
    operation_id = "list_entitlement_overrides",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 200, description = "Override rows", body = Vec<OverrideView>),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_entitlement_overrides(
    state: web::Data<PlatformAdminState>,
    path: web::Path<TenantId>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let rows = state
        .entitlements
        .list_overrides(path.into_inner())
        .await
        .map_err(api_error)?;
    let views: Vec<OverrideView> = rows
        .iter()
        .map(|row| OverrideView {
            tenant_id: row.tenant_id(),
            feature: row.feature().to_string(),
            enabled: row.enabled(),
            limit: row.limit(),
            expires_at: row.expires_at(),
            granted_by: row.granted_by(),
            granted_reason: row.granted_reason().map(str::to_string),
            updated_at: row.updated_at(),
        })
        .collect();
    Ok(HttpResponse::Ok().json(views))
}

/// `DELETE /api/v1/platform/tenants/{id}/entitlements/{feature}` — revoke
/// an override (fresh MFA required).
#[utoipa::path(
    delete,
    path = "/api/v1/platform/tenants/{id}/entitlements/{feature}",
    tag = "platform_admin",
    operation_id = "revoke_entitlement_override",
    params(
        ("id" = String, Path, description = "Tenant id"),
        ("feature" = String, Path, description = "Feature key")
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 403, description = "Permission denied or fresh MFA missing", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn revoke_entitlement_override(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    path: web::Path<(TenantId, String)>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<impl Responder, ApiError> {
    let (tenant_id, feature) = path.into_inner();
    state
        .entitlements
        .revoke_override(subject.0.user_id, tenant_id, feature)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::NoContent())
}

// ---------------------------------------------------------------------------
// Four-eyes approvals
// ---------------------------------------------------------------------------

/// `GET /api/v1/platform/approvals` — pending four-eyes requests.
#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListApprovalsQuery {
    /// Page size, clamped to 1..=200.
    #[param(schema_with = approval_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last approval of the previous page. Absent for
    /// the first page; the queue is ordered oldest first.
    pub after: Option<String>,
}

fn approval_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 200).build()
}

/// `GET /api/v1/platform/approvals` — pending four-eyes requests.
#[utoipa::path(
    get,
    path = "/api/v1/platform/approvals",
    tag = "platform_admin",
    operation_id = "list_pending_approvals",
    params(ListApprovalsQuery),
    responses(
        (status = 200, description = "Pending requests", body = Vec<crate::application::approvals::ApprovalView>),
        (status = 400, description = "Malformed cursor", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_pending_approvals(
    state: web::Data<PlatformAdminState>,
    query: web::Query<ListApprovalsQuery>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let approvals = state
        .list_approvals
        .execute(query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(approvals))
}

/// `POST /api/v1/platform/approvals/{id}/approve` — approve (executes the
/// recorded action) and record both identities.
#[utoipa::path(
    post,
    path = "/api/v1/platform/approvals/{id}/approve",
    tag = "platform_admin",
    operation_id = "approve_request",
    params(("id" = String, Path, description = "Approval id")),
    responses(
        (status = 200, description = "Approved and executed", body = crate::application::approvals::ApprovalView),
        (status = 403, description = "Permission denied, self-approval, or fresh MFA missing", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Approval not found", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Approval no longer pending", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn approve_request(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    path: web::Path<ApprovalId>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let approval = state
        .approve
        .execute(
            subject.0.user_id,
            sanvi_http::impersonation_actor(&req),
            DecideApprovalCommand {
                approval_id: path.into_inner(),
                approve: true,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(approval))
}

/// `POST /api/v1/platform/approvals/{id}/reject` — reject a request.
#[utoipa::path(
    post,
    path = "/api/v1/platform/approvals/{id}/reject",
    tag = "platform_admin",
    operation_id = "reject_request",
    params(("id" = String, Path, description = "Approval id")),
    responses(
        (status = 200, description = "Rejected", body = crate::application::approvals::ApprovalView),
        (status = 403, description = "Permission denied, self-approval, or fresh MFA missing", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Approval not found", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Approval no longer pending", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn reject_request(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    path: web::Path<ApprovalId>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let approval = state
        .reject
        .execute(
            subject.0.user_id,
            sanvi_http::impersonation_actor(&req),
            DecideApprovalCommand {
                approval_id: path.into_inner(),
                approve: false,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(approval))
}

// ---------------------------------------------------------------------------
// Application-fee configuration (09.6). The platform's own fee lives in
// the platform-admin context and its API — never the tenant API: a tenant
// cannot set the platform's fee. Every change is audited (actor+reason)
// and fresh-MFA gated like the other operator mutations.
// ---------------------------------------------------------------------------

/// One tenant's application-fee configuration (operator console).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApplicationFeeView {
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    /// Off everywhere by default in 0.10.0.
    pub enabled: bool,
    pub basis_points: Option<u32>,
    pub fixed_amount_minor: Option<i64>,
    pub fixed_currency: Option<String>,
    pub minimum_minor: i64,
}

fn fee_view(config: &crate::domain::fees::ApplicationFeeConfig) -> ApplicationFeeView {
    ApplicationFeeView {
        tenant_id: config.tenant_id,
        enabled: config.enabled,
        basis_points: config.basis_points,
        fixed_amount_minor: config.fixed_amount_minor,
        fixed_currency: config.fixed_currency.clone(),
        minimum_minor: config.minimum_minor,
    }
}

/// `GET /api/v1/platform/tenants/{id}/application-fee` — the stored
/// configuration. A never-configured tenant renders the disabled default
/// so the console has one shape to bind.
#[utoipa::path(
    get,
    path = "/api/v1/platform/tenants/{id}/application-fee",
    tag = "platform_admin",
    operation_id = "get_application_fee",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 200, description = "The configuration (disabled when never configured)", body = ApplicationFeeView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn get_application_fee(
    state: web::Data<PlatformAdminState>,
    path: web::Path<TenantId>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let fees = state
        .fees
        .as_ref()
        .ok_or_else(|| ApiError::internal("fee administration is not wired"))?;
    let tenant_id = path.into_inner();
    match fees.get(tenant_id).await.map_err(api_error)? {
        Some(config) => Ok(HttpResponse::Ok().json(fee_view(&config))),
        None => Ok(HttpResponse::Ok().json(ApplicationFeeView {
            tenant_id,
            enabled: false,
            basis_points: None,
            fixed_amount_minor: None,
            fixed_currency: None,
            minimum_minor: 0,
        })),
    }
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SetApplicationFeeRequest {
    /// Master switch. Enabling without a component is a validation error.
    #[serde(default)]
    pub enabled: bool,
    /// Basis points of the charge total (1..=10000).
    pub basis_points: Option<u32>,
    /// Flat component in minor units, applied to charges in its currency.
    pub fixed_amount_minor: Option<i64>,
    pub fixed_currency: Option<String>,
    /// Floor on the computed fee, minor units of the charge currency
    /// (currency-aware by interpretation: JPY minor units are whole yen;
    /// USD minor units are cents).
    #[serde(default)]
    pub minimum_minor: i64,
    /// Why the change is being made — audited with the actor.
    pub reason: Option<String>,
}

/// `PUT /api/v1/platform/tenants/{id}/application-fee` — create or update
/// the per-tenant fee configuration. Audited with actor + reason; takes
/// effect on the next checkout creation with no deploy (rollback is a
/// `DELETE` away).
#[utoipa::path(
    put,
    path = "/api/v1/platform/tenants/{id}/application-fee",
    tag = "platform_admin",
    operation_id = "set_application_fee",
    params(("id" = String, Path, description = "Tenant id")),
    request_body = SetApplicationFeeRequest,
    responses(
        (status = 201, description = "Configuration stored", body = ApplicationFeeView),
        (status = 400, description = "Invalid configuration", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied or fresh MFA missing", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn set_application_fee(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    path: web::Path<TenantId>,
    body: web::Json<SetApplicationFeeRequest>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<impl Responder, ApiError> {
    let fees = state
        .fees
        .as_ref()
        .ok_or_else(|| ApiError::internal("fee administration is not wired"))?;
    let config = crate::domain::fees::ApplicationFeeConfig {
        tenant_id: path.into_inner(),
        basis_points: body.basis_points,
        fixed_amount_minor: body.fixed_amount_minor,
        fixed_currency: body.fixed_currency.clone(),
        minimum_minor: body.minimum_minor,
        enabled: body.enabled,
    };
    fees.set(config.clone(), Some(subject.0.user_id), body.reason.clone())
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(fee_view(&config)))
}

/// `DELETE /api/v1/platform/tenants/{id}/application-fee` — disable (the
/// rollback path: data, no deploy; the numbers are retained for review).
#[utoipa::path(
    delete,
    path = "/api/v1/platform/tenants/{id}/application-fee",
    tag = "platform_admin",
    operation_id = "disable_application_fee",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 204, description = "Disabled"),
        (status = 403, description = "Permission denied or fresh MFA missing", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn disable_application_fee(
    state: web::Data<PlatformAdminState>,
    subject: CurrentSubject,
    path: web::Path<TenantId>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<impl Responder, ApiError> {
    let fees = state
        .fees
        .as_ref()
        .ok_or_else(|| ApiError::internal("fee administration is not wired"))?;
    fees.disable(path.into_inner(), Some(subject.0.user_id), None)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::NoContent())
}
