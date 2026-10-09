use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{ApiError, CurrentSubject, CurrentTenant, FreshAal2Required, RequiredPermission};
use sanvi_kernel::{RoleId, TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::application::{
    CreateCustomRole, CreateCustomRoleCommand, DefineFeature, DefineFeatureCommand,
    DeleteCustomRole, DeprecateFeature, ListFeatures, ListPermissions, ListRoles,
    ListTenantEntitlements, PermissionView, UpdateCustomRole, UpdateCustomRoleCommand,
    UpdateFeature, UpdateFeatureCommand,
};
use crate::domain::features::{Feature, FeatureKey, FeatureKind, Visibility};
use crate::domain::roles::{Role, RoleScope};
use crate::domain::{AccessError, ResolvedEntitlement};

use super::validation;

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct AccessState {
    pub list_permissions: Arc<ListPermissions>,
    pub list_roles: Arc<ListRoles>,
    pub create_role: Arc<CreateCustomRole>,
    pub update_role: Arc<UpdateCustomRole>,
    pub delete_role: Arc<DeleteCustomRole>,
    pub list_features: Arc<ListFeatures>,
    pub define_feature: Arc<DefineFeature>,
    pub update_feature: Arc<UpdateFeature>,
    pub deprecate_feature: Arc<DeprecateFeature>,
    pub list_tenant_entitlements: Arc<ListTenantEntitlements>,
}

/// The access context's domain-error → HTTP mapping (single place, this
/// boundary).
pub fn api_error(err: AccessError) -> ApiError {
    match err {
        // TASK-027 (FR-1127): invalid-input rejections are field-addressed
        // (`validation::validation_issue` owns the field paths).
        AccessError::InvalidPermission(_)
        | AccessError::InvalidRoleScope(_)
        | AccessError::InvalidRoleKey(_)
        | AccessError::InvalidFeatureKey(_)
        | AccessError::InvalidFeatureDefinition(_)
        | AccessError::InvalidEntitlement(_)
        | AccessError::InvalidLimit(_)
        | AccessError::PermissionScopeMismatch { .. } => {
            let (detail, issues) =
                validation::validation_issue(&err).expect("input rejections are field-addressed");
            ApiError::validation(detail, issues)
        }
        AccessError::RoleAlreadyExists { key } | AccessError::FeatureAlreadyExists { key } => {
            ApiError::Conflict {
                detail: format!("'{key}' already exists"),
            }
        }
        AccessError::RoleNotFound { key }
        | AccessError::FeatureNotFound { key }
        | AccessError::EntitlementNotFound { feature: key } => ApiError::NotFound { resource: key },
        AccessError::RoleKeyResolutionFailed(role_id) => ApiError::NotFound {
            resource: format!("role {role_id}"),
        },
        AccessError::SystemRoleImmutable { key } => ApiError::Forbidden {
            detail: format!("role '{key}' is a system role and cannot be modified"),
        },
        AccessError::Escalation { permission } => ApiError::Forbidden {
            detail: format!("cannot grant '{permission}': you do not hold it"),
        },
        AccessError::QuotaExceeded {
            feature,
            limit,
            attempted,
        } => ApiError::Conflict {
            detail: format!("quota exceeded for {feature}: limit {limit}, attempted {attempted}"),
        },
        AccessError::UnknownPermission(permission) => ApiError::NotFound {
            resource: format!("permission {permission}"),
        },
        AccessError::EntitlementBackendUnavailable(detail)
        | AccessError::BackendUnreachable(detail)
        | AccessError::BackendRejected(detail) => {
            tracing::error!(error = %detail, "access backend failure");
            ApiError::ServiceUnavailable {
                detail: "access control is temporarily unavailable".to_string(),
            }
        }
        AccessError::Storage(detail) => {
            tracing::error!(error = %detail, "access storage failure");
            ApiError::from_storage_detail(&detail)
        }
        AccessError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "access ID generation failure");
            ApiError::ServiceUnavailable {
                detail: "ID generation is temporarily unavailable".to_string(),
            }
        }
    }
}

/// `GET /api/v1/access/permissions` — the central permission registry,
/// for the admin/UI contracts. Readable by any authenticated subject.
#[utoipa::path(
    get,
    path = "/api/v1/access/permissions",
    tag = "access",
    operation_id = "list_permissions",
    responses(
        (status = 200, description = "Permission registry", body = Vec<PermissionView>),
        (status = 401, description = "Not authenticated", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_permissions(
    state: web::Data<AccessState>,
    _guard: RequiredPermission,
) -> impl Responder {
    HttpResponse::Ok().json(state.list_permissions.execute())
}

// ---------------------------------------------------------------------------
// Roles: tenant authoring + platform catalog.
// ---------------------------------------------------------------------------

/// One role as exposed by the API.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RoleView {
    #[schema(value_type = String)]
    pub id: RoleId,
    pub scope: RoleScope,
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    pub key: String,
    pub name: String,
    pub permissions: Vec<String>,
    pub is_system: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl From<&Role> for RoleView {
    fn from(role: &Role) -> Self {
        Self {
            id: role.id(),
            scope: role.scope(),
            tenant_id: role.tenant_id(),
            key: role.key().to_string(),
            name: role.name().to_string(),
            permissions: role.permissions().iter().cloned().collect(),
            is_system: role.is_system(),
            created_at: role.created_at(),
        }
    }
}

/// `GET /api/v1/platform/roles` — the full role catalog (operator console).
#[utoipa::path(
    get,
    path = "/api/v1/platform/roles",
    tag = "access",
    operation_id = "list_roles_platform",
    responses(
        (status = 200, description = "Role catalog", body = Vec<RoleView>),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_roles_platform(
    state: web::Data<AccessState>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let roles = state
        .list_roles
        .execute_platform()
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(roles.iter().map(RoleView::from).collect::<Vec<_>>()))
}

/// `GET /api/v1/tenant/roles` — roles assignable in the current tenant.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/roles",
    tag = "access",
    operation_id = "list_roles_tenant",
    responses(
        (status = 200, description = "Assignable roles", body = Vec<RoleView>),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_roles_tenant(
    state: web::Data<AccessState>,
    tenant: CurrentTenant,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let roles = state
        .list_roles
        .execute_tenant(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(roles.iter().map(RoleView::from).collect::<Vec<_>>()))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateRoleRequest {
    pub key: String,
    pub name: String,
    pub permissions: Vec<String>,
}

/// `POST /api/v1/tenant/roles` — create a custom tenant role. The role may
/// only grant permissions the caller already holds (no escalation).
#[utoipa::path(
    post,
    path = "/api/v1/tenant/roles",
    tag = "access",
    operation_id = "create_role",
    request_body = CreateRoleRequest,
    responses(
        (status = 201, description = "Role created", body = RoleView),
        (status = 400, description = "Invalid role definition", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Escalation or permission denied", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Role key already exists", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn create_role(
    state: web::Data<AccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    body: web::Json<CreateRoleRequest>,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let actor = subject.0.user_id;
    let on_behalf_of = on_behalf_of(&req);
    let role = state
        .create_role
        .execute(
            actor,
            on_behalf_of,
            CreateCustomRoleCommand {
                tenant_id: tenant.0.tenant_id,
                key: body.key.clone(),
                name: body.name.clone(),
                permissions: body.permissions.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(RoleView::from(&role)))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateRoleRequest {
    pub name: Option<String>,
    pub permissions: Option<Vec<String>>,
}

/// `PATCH /api/v1/tenant/roles/{id}` — update a custom tenant role.
#[utoipa::path(
    patch,
    path = "/api/v1/tenant/roles/{id}",
    tag = "access",
    operation_id = "update_role",
    params(("id" = String, Path, description = "Role id")),
    request_body = UpdateRoleRequest,
    responses(
        (status = 200, description = "Role updated", body = RoleView),
        (status = 400, description = "Invalid role definition", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Escalation or permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Role not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn update_role(
    state: web::Data<AccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<String>,
    body: web::Json<UpdateRoleRequest>,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let actor = subject.0.user_id;
    let on_behalf_of = on_behalf_of(&req);
    let role_id = role_id(&path)?;
    let role = state
        .update_role
        .execute(
            actor,
            on_behalf_of,
            UpdateCustomRoleCommand {
                role_id,
                tenant_id: tenant.0.tenant_id,
                name: body.name.clone(),
                permissions: body.permissions.clone(),
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(RoleView::from(&role)))
}

/// `DELETE /api/v1/tenant/roles/{id}` — delete a custom tenant role.
#[utoipa::path(
    delete,
    path = "/api/v1/tenant/roles/{id}",
    tag = "access",
    operation_id = "delete_role",
    params(("id" = String, Path, description = "Role id")),
    responses(
        (status = 204, description = "Role deleted"),
        (status = 403, description = "System role or permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Role not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn delete_role(
    state: web::Data<AccessState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    path: web::Path<String>,
    _guard: RequiredPermission,
    req: HttpRequest,
) -> Result<impl Responder, ApiError> {
    let actor = subject.0.user_id;
    let on_behalf_of = on_behalf_of(&req);
    let role_id = role_id(&path)?;
    state
        .delete_role
        .execute(actor, on_behalf_of, tenant.0.tenant_id, role_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::NoContent())
}

// ---------------------------------------------------------------------------
// Features: the catalog authoring endpoints (platform operators).
// ---------------------------------------------------------------------------

/// One catalog feature as exposed by the API.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FeatureView {
    pub key: String,
    pub name: String,
    pub kind: FeatureKind,
    pub default_enabled: bool,
    pub default_limit: Option<i64>,
    pub visibility: Visibility,
    pub deprecated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<&Feature> for FeatureView {
    fn from(feature: &Feature) -> Self {
        Self {
            key: feature.key().to_string(),
            name: feature.name().to_string(),
            kind: feature.kind(),
            default_enabled: feature.default_enabled(),
            default_limit: feature.default_limit(),
            visibility: feature.visibility(),
            deprecated_at: feature.deprecated_at(),
        }
    }
}

/// `GET /api/v1/platform/features` — the feature catalog.
#[utoipa::path(
    get,
    path = "/api/v1/platform/features",
    tag = "access",
    operation_id = "list_features",
    responses(
        (status = 200, description = "Feature catalog", body = Vec<FeatureView>),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_features(
    state: web::Data<AccessState>,
    _subject: CurrentSubject,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let features = state.list_features.execute().await.map_err(api_error)?;
    Ok(HttpResponse::Ok().json(features.iter().map(FeatureView::from).collect::<Vec<_>>()))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DefineFeatureRequest {
    pub key: String,
    pub name: String,
    pub kind: FeatureKind,
    #[serde(default)]
    pub default_enabled: bool,
    pub default_limit: Option<i64>,
    #[serde(default = "default_visibility")]
    pub visibility: Visibility,
}

fn default_visibility() -> Visibility {
    Visibility::Public
}

/// `POST /api/v1/platform/features` — define a new feature.
#[utoipa::path(
    post,
    path = "/api/v1/platform/features",
    tag = "access",
    operation_id = "define_feature",
    request_body = DefineFeatureRequest,
    responses(
        (status = 201, description = "Feature defined", body = FeatureView),
        (status = 400, description = "Invalid definition", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 409, description = "Feature key already exists", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn define_feature(
    state: web::Data<AccessState>,
    subject: CurrentSubject,
    body: web::Json<DefineFeatureRequest>,
    _guard: RequiredPermission,
) -> Result<impl Responder, ApiError> {
    let feature = state
        .define_feature
        .execute(
            subject.0.user_id,
            None,
            DefineFeatureCommand {
                key: FeatureKey::parse(&body.key).map_err(api_error)?,
                name: body.name.clone(),
                kind: body.kind,
                default_enabled: body.default_enabled,
                default_limit: body.default_limit,
                visibility: body.visibility,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Created().json(FeatureView::from(&feature)))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UpdateFeatureRequest {
    pub name: String,
    pub default_enabled: bool,
    pub default_limit: Option<i64>,
    #[serde(default = "default_visibility")]
    pub visibility: Visibility,
}

/// `PATCH /api/v1/platform/features/{key}` — update a feature's defaults.
#[utoipa::path(
    patch,
    path = "/api/v1/platform/features/{key}",
    tag = "access",
    operation_id = "update_feature",
    params(("key" = String, Path, description = "Feature key")),
    request_body = UpdateFeatureRequest,
    responses(
        (status = 200, description = "Feature updated", body = FeatureView),
        (status = 400, description = "Invalid definition", body = sanvi_http::ProblemDetail),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Feature not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn update_feature(
    state: web::Data<AccessState>,
    subject: CurrentSubject,
    path: web::Path<String>,
    body: web::Json<UpdateFeatureRequest>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<impl Responder, ApiError> {
    let feature = state
        .update_feature
        .execute(
            subject.0.user_id,
            None,
            UpdateFeatureCommand {
                key: FeatureKey::parse(&path.into_inner()).map_err(api_error)?,
                name: body.name.clone(),
                default_enabled: body.default_enabled,
                default_limit: body.default_limit,
                visibility: body.visibility,
            },
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(FeatureView::from(&feature)))
}

/// `DELETE /api/v1/platform/features/{key}` — deprecate a feature (it stays
/// resolvable; it stops appearing in tenant-facing catalogs).
#[utoipa::path(
    delete,
    path = "/api/v1/platform/features/{key}",
    tag = "access",
    operation_id = "deprecate_feature",
    params(("key" = String, Path, description = "Feature key")),
    responses(
        (status = 200, description = "Feature deprecated", body = FeatureView),
        (status = 403, description = "Permission denied", body = sanvi_http::ProblemDetail),
        (status = 404, description = "Feature not found", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn deprecate_feature(
    state: web::Data<AccessState>,
    subject: CurrentSubject,
    path: web::Path<String>,
    _guard: RequiredPermission,
    _fresh_mfa: FreshAal2Required,
) -> Result<impl Responder, ApiError> {
    let feature = state
        .deprecate_feature
        .execute(
            subject.0.user_id,
            None,
            &FeatureKey::parse(&path.into_inner()).map_err(api_error)?,
        )
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(FeatureView::from(&feature)))
}

/// One resolved entitlement as exposed to a tenant.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResolvedEntitlementView {
    pub feature: String,
    pub kind: FeatureKind,
    pub enabled: bool,
    pub limit: Option<i64>,
    pub source: EntitlementSourceView,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementSourceView {
    Override,
    Subscription,
    PlanDefault,
    FeatureDefault,
}

impl From<&ResolvedEntitlement> for ResolvedEntitlementView {
    fn from(entry: &ResolvedEntitlement) -> Self {
        Self {
            feature: entry.feature.to_string(),
            kind: entry.kind,
            enabled: entry.enabled,
            limit: entry.limit,
            source: match entry.source {
                crate::domain::EntitlementSource::Override => EntitlementSourceView::Override,
                crate::domain::EntitlementSource::Subscription => {
                    EntitlementSourceView::Subscription
                }
                crate::domain::EntitlementSource::PlanDefault => EntitlementSourceView::PlanDefault,
                crate::domain::EntitlementSource::FeatureDefault => {
                    EntitlementSourceView::FeatureDefault
                }
            },
            expires_at: entry.expires_at,
        }
    }
}

/// `GET /api/v1/tenant/entitlements` — the tenant's own resolved
/// entitlements (feature-gated on `access.tenant_entitlements`).
#[utoipa::path(
    get,
    path = "/api/v1/tenant/entitlements",
    tag = "access",
    operation_id = "list_tenant_entitlements",
    responses(
        (status = 200, description = "Resolved entitlements", body = Vec<ResolvedEntitlementView>),
        (status = 403, description = "Permission denied or feature not enabled", body = sanvi_http::ProblemDetail),
        (status = 404, description = "No tenant context", body = sanvi_http::ProblemDetail)
    )
)]
pub async fn list_tenant_entitlements(
    state: web::Data<AccessState>,
    tenant: CurrentTenant,
    _guard: RequiredPermission,
    _feature: sanvi_http::RequiredFeature,
) -> Result<impl Responder, ApiError> {
    let entries = state
        .list_tenant_entitlements
        .execute(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(
        entries
            .iter()
            .map(ResolvedEntitlementView::from)
            .collect::<Vec<_>>(),
    ))
}

// Helpers ---------------------------------------------------------------------

/// Parse a role id path segment.
fn role_id(path: &web::Path<String>) -> Result<RoleId, ApiError> {
    path.parse::<RoleId>()
        .map_err(|_| ApiError::bad_request("invalid role id"))
}

/// The actor to record in audit entries as `on_behalf_of`: the
/// impersonating operator, when the request arrived through an
/// impersonation grant (resolved by the impersonation middleware).
fn on_behalf_of(req: &actix_web::HttpRequest) -> Option<UserId> {
    sanvi_http::impersonation_actor(req)
}
