//! Handlers: thin actix shims over the application layer.
//!
//! Handlers know nothing about Postgres or Redis; they resolve use cases
//! from application state and translate domain errors into [`ApiError`] at
//! this boundary (the tenancy context's slice of the single mapping rule).

use std::sync::Arc;
use std::time::Duration;

use actix_web::{HttpMessage, HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{
    ApiError, CurrentTenant, OptionalSubject, ProblemDetail, RequiredPermission, TenantContext,
};
use sanvi_persistence::IdempotencyStore;
use serde::Deserialize;

use crate::adapters::resolution::HostTenantResolver;
use crate::application::{
    ActivateTenant, ArchiveTenant, CheckSlugAvailability, GetTenant, GetTenantSettings,
    ListTenants, ProvisionTenant, ProvisionTenantCommand, ResolveTenant, ResumeTenant,
    SettingsUpdate, SlugAvailability, SuspendTenant, SuspendTenantRequest, TenantListView,
    TenantSettingsOutput, TenantView, UpdateTenantSettings,
};
use crate::domain::TenancyError;
use crate::domain::tenant::{TenantId, TenantStatus};
use crate::domain::values::SuspensionReason;

/// Application state visible to this context's handlers.
#[derive(Clone)]
pub struct TenancyState {
    pub quotas: Option<Arc<sanvi_http::quotas::QuotaService>>,
    pub provision: Arc<ProvisionTenant>,
    pub activate: Arc<ActivateTenant>,
    pub suspend: Arc<SuspendTenant>,
    pub resume: Arc<ResumeTenant>,
    pub archive: Arc<ArchiveTenant>,
    pub get_tenant: Arc<GetTenant>,
    pub list_tenants: Arc<ListTenants>,
    pub get_settings: Arc<GetTenantSettings>,
    pub update_settings: Arc<UpdateTenantSettings>,
    pub slug_availability: Arc<CheckSlugAvailability>,
    pub resolve: Arc<ResolveTenant>,
    pub host_resolver: Arc<HostTenantResolver>,
    pub idempotency: IdempotencyStore,
}

/// The tenancy context's domain-error → HTTP mapping (single place, this
/// boundary).
pub fn api_error(err: TenancyError) -> ApiError {
    match err {
        // TASK-027 (FR-1127): invalid-input rejections are field-addressed
        // — the flat message is unchanged, the offending input is named on
        // top. `validation::validation_issue` owns the field paths.
        TenancyError::InvalidSlug(_)
        | TenancyError::InvalidDisplayName(_)
        | TenancyError::InvalidRegion(_)
        | TenancyError::InvalidLocale(_)
        | TenancyError::InvalidSuspensionReason(_)
        | TenancyError::InvalidSettings(_) => {
            let (detail, issues) = super::validation::validation_issue(&err)
                .expect("input rejections are field-addressed");
            ApiError::validation(detail, issues)
        }
        TenancyError::SlugReserved { slug } | TenancyError::SlugTaken { slug } => {
            ApiError::Conflict {
                detail: format!("slug '{slug}' is unavailable"),
            }
        }
        TenancyError::IllegalTransition { from, to } => ApiError::Conflict {
            detail: format!("illegal tenant state transition: {from} -> {to}"),
        },
        // TASK-028 (FR-1131): the offboarding state machine's refusals
        // are conflicts naming the request — not 5xx, not silent.
        TenancyError::OffboardingTransition {
            request_id,
            from,
            to,
        } => ApiError::ConflictReasoned {
            detail: format!("offboarding request {request_id}: illegal transition {from} -> {to}"),
            reason: "offboarding-transition".to_string(),
        },
        TenancyError::OffboardingExportMissing { request_id } => ApiError::ConflictReasoned {
            detail: format!(
                "offboarding request {request_id}: the purge is unreachable before \
                 the export verifies (FR-1131)"
            ),
            reason: "offboarding-export-missing".to_string(),
        },
        TenancyError::Conflict(detail) => ApiError::Conflict { detail },
        TenancyError::TenantNotFound { id } => ApiError::NotFound {
            resource: format!("tenant {id}"),
        },
        TenancyError::MissingTenantContext => ApiError::NotFound {
            resource: "tenant".to_string(),
        },
        TenancyError::InvalidStatus(detail) => ApiError::internal(detail),
        TenancyError::Storage(detail) => {
            tracing::error!(error = %detail, "tenancy storage failure");
            ApiError::from_storage_detail(&detail)
        }
        TenancyError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "tenancy ID generation failure");
            ApiError::internal("ID generation failure")
        }
    }
}

/// Parse a tenant id path segment.
fn tenant_id(path: &web::Path<String>) -> Result<TenantId, ApiError> {
    path.parse::<TenantId>()
        .map_err(|_| ApiError::bad_request("invalid tenant id"))
}

/// Idempotency window for provisioning.
const IDEMPOTENCY_TTL: Duration = Duration::from_secs(60);

/// `POST /api/v1/platform/tenants` — provision a new tenant.
#[utoipa::path(
    get, path = "/api/v1/tenant/usage", tag = "tenancy", operation_id = "get_tenant_usage",
    responses(
        (status = 200, description = "Current-period usage and plan limits", body = sanvi_http::quotas::TenantUsage),
        (status = 401, description = "Authentication required", body = ProblemDetail),
        (status = 403, description = "Usage permission required", body = ProblemDetail),
        (status = 503, description = "Counters unavailable", body = ProblemDetail)
    )
)]
pub async fn get_usage(
    state: web::Data<TenancyState>,
    current: CurrentTenant,
    _subject: sanvi_http::CurrentSubject,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let counters = state
        .quotas
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("usage unavailable"))?;
    Ok(HttpResponse::Ok().json(
        counters
            .usage(current.0.tenant_id)
            .await
            .map_err(ApiError::from)?,
    ))
}

/// `POST /api/v1/platform/tenants` — provision a new tenant.
#[utoipa::path(
    post,
    path = "/api/v1/platform/tenants",
    tag = "tenancy",
    operation_id = "provision_tenant",
    request_body = ProvisionTenantCommand,
    responses(
        (status = 201, description = "Tenant provisioned", body = TenantView),
        (status = 400, description = "Invalid input", body = ProblemDetail),
        (status = 409, description = "Slug unavailable or illegal transition", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn provision_tenant(
    state: web::Data<TenancyState>,
    body: web::Json<ProvisionTenantCommand>,
    req: HttpRequest,
    _guard: RequiredPermission,
) -> impl Responder {
    let cmd = body.into_inner();
    let request_id = req
        .extensions()
        .get::<sanvi_http::RequestId>()
        .map(|id| id.to_string());
    let instance = req.path().to_string();
    let idempotency_key = req
        .headers()
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    // Claim first: a retried provision replays the stored response instead
    // of racing the original.
    const SCOPE: &str = "tenancy.provision";
    if let Some(key) = &idempotency_key {
        match state.idempotency.claim(key, SCOPE, IDEMPOTENCY_TTL).await {
            Ok(Some(hit)) => {
                let status = actix_web::http::StatusCode::from_u16(hit.status)
                    .unwrap_or(actix_web::http::StatusCode::OK);
                let body = hit.response.unwrap_or_default();
                return HttpResponse::build(status)
                    .content_type("application/json")
                    .body(body.to_string());
            }
            Ok(None) => {}
            Err(err) => {
                tracing::error!(error = %err, "idempotency claim failed");
                return HttpResponse::InternalServerError().json(
                    ApiError::internal("idempotency backend unavailable")
                        .with_context(Some(instance), request_id),
                );
            }
        }
    }

    match state.provision.execute(cmd).await {
        Ok(view) => {
            let body = serde_json::json!(view);
            if let Some(key) = &idempotency_key
                && let Err(err) = state
                    .idempotency
                    .resolve(key, SCOPE, 201, body.clone())
                    .await
            {
                tracing::warn!(error = %err, "failed to store idempotent response");
            }
            HttpResponse::Created().json(view)
        }
        Err(err) => {
            if let Some(key) = &idempotency_key
                && let Err(err) = state.idempotency.release(key, SCOPE).await
            {
                tracing::warn!(error = %err, "failed to release idempotency key");
            }
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.with_context(Some(instance), request_id))
        }
    }
}

/// `GET /api/v1/platform/tenants` — list tenants (cursor pagination).
/// Ordered by created_at DESC, id DESC.
#[utoipa::path(
    get,
    path = "/api/v1/platform/tenants",
    tag = "tenancy",
    operation_id = "list_tenants",
    params(ListQuery),
    responses(
        (status = 200, description = "Tenant page", body = TenantListView),
        (status = 400, description = "Invalid filter", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail),
        (status = 504, description = "Statement timeout — the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_tenants(
    state: web::Data<TenancyState>,
    query: web::Query<ListQuery>,
    _guard: RequiredPermission,
) -> impl Responder {
    let status = match query.status.as_deref() {
        Some(raw) => match TenantStatus::parse(raw) {
            Ok(status) => Some(status),
            Err(err) => return HttpResponse::BadRequest().json(api_error(err).problem()),
        },
        None => None,
    };
    let after = match query.after.as_deref() {
        Some(raw) => match raw.parse::<TenantId>() {
            Ok(id) => Some(id),
            Err(_) => {
                return HttpResponse::BadRequest()
                    .json(ApiError::bad_request("invalid cursor").problem());
            }
        },
        None => None,
    };
    match state.list_tenants.execute(after, query.limit, status).await {
        Ok(page) => HttpResponse::Ok().json(page),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Page size, clamped to 1..=100.
    #[param(schema_with = tenant_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: tenant id of the last item of the previous page.
    pub after: Option<String>,
    /// Filter by tenant status.
    pub status: Option<String>,
}

fn tenant_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(100, 20).build()
}

/// `GET /api/v1/platform/tenants/{id}` — tenant detail.
#[utoipa::path(
    get,
    path = "/api/v1/platform/tenants/{id}",
    tag = "tenancy",
    operation_id = "get_tenant",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 200, description = "Tenant", body = TenantView),
        (status = 404, description = "Tenant not found", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn get_tenant(
    state: web::Data<TenancyState>,
    path: web::Path<String>,
    _guard: RequiredPermission,
) -> impl Responder {
    let id = match tenant_id(&path) {
        Ok(id) => id,
        Err(err) => return HttpResponse::BadRequest().json(err.problem()),
    };
    match state.get_tenant.execute(id).await {
        Ok(view) => HttpResponse::Ok().json(view),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `POST /api/v1/platform/tenants/{id}/activate` — provisioning → active.
#[utoipa::path(
    post,
    path = "/api/v1/platform/tenants/{id}/activate",
    tag = "tenancy",
    operation_id = "activate_tenant",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 200, description = "Tenant activated", body = TenantView),
        (status = 404, description = "Tenant not found", body = ProblemDetail),
        (status = 409, description = "Illegal transition", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn activate_tenant(
    state: web::Data<TenancyState>,
    path: web::Path<String>,
    _guard: RequiredPermission,
) -> impl Responder {
    let id = match tenant_id(&path) {
        Ok(id) => id,
        Err(err) => return HttpResponse::BadRequest().json(err.problem()),
    };
    lifecycle_response(state.activate.execute(id).await)
}

/// `POST /api/v1/platform/tenants/{id}/suspend` — active → suspended.
#[utoipa::path(
    post,
    path = "/api/v1/platform/tenants/{id}/suspend",
    tag = "tenancy",
    operation_id = "suspend_tenant",
    params(("id" = String, Path, description = "Tenant id")),
    request_body = SuspendTenantRequest,
    responses(
        (status = 200, description = "Tenant suspended", body = TenantView),
        (status = 404, description = "Tenant not found", body = ProblemDetail),
        (status = 409, description = "Illegal transition", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn suspend_tenant(
    state: web::Data<TenancyState>,
    path: web::Path<String>,
    body: web::Json<SuspendBody>,
    _guard: RequiredPermission,
) -> impl Responder {
    let id = match tenant_id(&path) {
        Ok(id) => id,
        Err(err) => return HttpResponse::BadRequest().json(err.problem()),
    };
    let reason = match SuspensionReason::parse(&body.reason) {
        Ok(reason) => reason,
        Err(err) => return HttpResponse::BadRequest().json(api_error(err).problem()),
    };
    lifecycle_response(state.suspend.execute(id, reason).await)
}

/// `POST /api/v1/platform/tenants/{id}/resume` — suspended → active.
#[utoipa::path(
    post,
    path = "/api/v1/platform/tenants/{id}/resume",
    tag = "tenancy",
    operation_id = "resume_tenant",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 200, description = "Tenant resumed", body = TenantView),
        (status = 404, description = "Tenant not found", body = ProblemDetail),
        (status = 409, description = "Illegal transition", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn resume_tenant(
    state: web::Data<TenancyState>,
    path: web::Path<String>,
    _guard: RequiredPermission,
) -> impl Responder {
    let id = match tenant_id(&path) {
        Ok(id) => id,
        Err(err) => return HttpResponse::BadRequest().json(err.problem()),
    };
    lifecycle_response(state.resume.execute(id).await)
}

/// `POST /api/v1/platform/tenants/{id}/archive` — active|suspended → archived.
#[utoipa::path(
    post,
    path = "/api/v1/platform/tenants/{id}/archive",
    tag = "tenancy",
    operation_id = "archive_tenant",
    params(("id" = String, Path, description = "Tenant id")),
    responses(
        (status = 200, description = "Tenant archived", body = TenantView),
        (status = 404, description = "Tenant not found", body = ProblemDetail),
        (status = 409, description = "Illegal transition", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn archive_tenant(
    state: web::Data<TenancyState>,
    path: web::Path<String>,
    _guard: RequiredPermission,
) -> impl Responder {
    let id = match tenant_id(&path) {
        Ok(id) => id,
        Err(err) => return HttpResponse::BadRequest().json(err.problem()),
    };
    lifecycle_response(state.archive.execute(id).await)
}

fn lifecycle_response(
    result: Result<crate::application::TenantView, TenancyError>,
) -> HttpResponse {
    match result {
        Ok(view) => HttpResponse::Ok().json(view),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `GET /api/v1/tenant/context` — the current tenant's self-description
/// (resolved by the middleware; the frontend bootstrap endpoint). Public,
/// with the signed-in subject attached when a session exists.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/context",
    tag = "tenancy",
    operation_id = "get_tenant_context",
    responses(
        (status = 200, description = "Resolved tenant context", body = TenantContext),
        (status = 404, description = "No tenant resolved", body = ProblemDetail)
    )
)]
pub async fn get_context(current: CurrentTenant, _subject: OptionalSubject) -> impl Responder {
    HttpResponse::Ok().json(current.0)
}

/// `GET /api/v1/tenant/settings` — all settings of the current tenant.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/settings",
    tag = "tenancy",
    operation_id = "get_tenant_settings",
    responses(
        (status = 200, description = "Tenant settings", body = TenantSettingsOutput),
        (status = 401, description = "Not authenticated", body = ProblemDetail),
        (status = 403, description = "Permission denied", body = ProblemDetail),
        (status = 404, description = "No tenant resolved", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn get_settings(
    state: web::Data<TenancyState>,
    current: CurrentTenant,
    _guard: RequiredPermission,
) -> impl Responder {
    let id = match TenantId::try_from(current.0.tenant_id) {
        Ok(id) => id,
        Err(_) => {
            return HttpResponse::BadRequest()
                .json(ApiError::bad_request("invalid tenant id").problem());
        }
    };
    match state.get_settings.execute(id).await {
        Ok(output) => HttpResponse::Ok().json(output),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `PUT /api/v1/tenant/settings` — upsert settings of the current tenant.
#[utoipa::path(
    put,
    path = "/api/v1/tenant/settings",
    tag = "tenancy",
    operation_id = "update_tenant_settings",
    request_body = SettingsUpdate,
    responses(
        (status = 200, description = "Updated settings", body = TenantSettingsOutput),
        (status = 400, description = "Invalid settings", body = ProblemDetail),
        (status = 401, description = "Not authenticated", body = ProblemDetail),
        (status = 403, description = "Permission denied", body = ProblemDetail),
        (status = 404, description = "No tenant resolved", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn put_settings(
    state: web::Data<TenancyState>,
    current: CurrentTenant,
    body: web::Json<SettingsBody>,
    _guard: RequiredPermission,
) -> impl Responder {
    let id = match TenantId::try_from(current.0.tenant_id) {
        Ok(id) => id,
        Err(_) => {
            return HttpResponse::BadRequest()
                .json(ApiError::bad_request("invalid tenant id").problem());
        }
    };
    match state
        .update_settings
        .execute(
            id,
            crate::application::SettingsUpdate {
                settings: body.settings.clone(),
            },
        )
        .await
    {
        Ok(output) => HttpResponse::Ok().json(output),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `GET /api/v1/public/slug-availability` — is this slug free? (boolean only.)
#[utoipa::path(
    get,
    path = "/api/v1/public/slug-availability",
    tag = "tenancy",
    operation_id = "check_slug_availability",
    params(("slug" = String, Query, description = "Candidate slug")),
    responses(
        (status = 200, description = "Availability answer", body = SlugAvailability),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn slug_availability(
    state: web::Data<TenancyState>,
    query: web::Query<SlugQuery>,
) -> impl Responder {
    match state.slug_availability.execute(&query.slug).await {
        Ok(answer) => HttpResponse::Ok().json(answer),
        Err(err) => {
            let error = api_error(err);
            HttpResponse::build(error.status()).json(error.problem())
        }
    }
}

/// `GET /api/v1/public/tenant-context` — host-based context for the
/// storefront SSR bootstrap. Unknown host → 404; suspended tenants answer
/// normally (the frontend renders the maintenance page).
#[utoipa::path(
    get,
    path = "/api/v1/public/tenant-context",
    tag = "tenancy",
    operation_id = "public_tenant_context",
    responses(
        (status = 200, description = "Tenant context for this host", body = TenantContext),
        (status = 404, description = "Unknown host", body = ProblemDetail),
        (status = 500, description = "Internal error", body = ProblemDetail)
    )
)]
pub async fn public_tenant_context(
    state: web::Data<TenancyState>,
    req: HttpRequest,
) -> impl Responder {
    let host = req
        .headers()
        .get(actix_web::http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let instance = req.path().to_string();
    let request_id = req
        .extensions()
        .get::<sanvi_http::RequestId>()
        .map(|id| id.to_string());
    match state.host_resolver.resolve_public(&host).await {
        Ok(Some(context)) => HttpResponse::Ok().json(context),
        Ok(None) => {
            let problem = ApiError::NotFound {
                resource: "tenant".to_string(),
            }
            .with_context(Some(instance), request_id);
            HttpResponse::NotFound().json(problem)
        }
        Err(err) => {
            HttpResponse::build(err.status()).json(err.with_context(Some(instance), request_id))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SlugQuery {
    pub slug: String,
}

#[derive(Debug, Deserialize)]
pub struct SuspendBody {
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct SettingsBody {
    pub settings: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Convenience for tests: state whose handlers are never invoked.
pub fn state_for_tests(ids: Arc<dyn sanvi_kernel::RawIdGenerator>) -> TenancyState {
    let tx_manager = sanvi_persistence::TxManager::new(
        sanvi_persistence::PgPool::connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
            .expect("lazy pool construction never connects"),
        ids,
    );
    let tenants: Arc<dyn crate::domain::TenantRepository> = Arc::new(UnreachableRepo);
    let settings: Arc<dyn crate::domain::TenantSettingsRepository> = Arc::new(UnreachableSettings);
    let slugs: Arc<dyn crate::domain::SlugReservationPort> = Arc::new(UnreachableSlugs);
    let events: Arc<dyn crate::domain::EventPublisher> = Arc::new(UnreachableEvents);
    let ids: Arc<dyn crate::domain::IdGenerator> = Arc::new(UnreachableIds);
    let clock: Arc<dyn crate::domain::Clock> = Arc::new(UnreachableClock);
    let cache: Arc<dyn crate::domain::TenantResolutionCache> = Arc::new(UnreachableCache);

    let resolve = Arc::new(ResolveTenant::new(
        tx_manager.clone(),
        tenants.clone(),
        cache.clone(),
    ));
    let host_resolver = Arc::new(HostTenantResolver::new(
        resolve.clone(),
        "platform.sanvi.app".to_string(),
        Vec::new(),
    ));

    TenancyState {
        quotas: None,
        provision: Arc::new(ProvisionTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            slugs.clone(),
            events.clone(),
            ids,
            clock.clone(),
        )),
        activate: Arc::new(ActivateTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        suspend: Arc::new(SuspendTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        resume: Arc::new(ResumeTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        archive: Arc::new(ArchiveTenant::new(
            tx_manager.clone(),
            tenants.clone(),
            events.clone(),
            clock.clone(),
        )),
        get_tenant: Arc::new(GetTenant::new(tx_manager.clone(), tenants.clone())),
        list_tenants: Arc::new(ListTenants::new(tx_manager.clone(), tenants.clone())),
        get_settings: Arc::new(GetTenantSettings::new(tx_manager.clone(), settings.clone())),
        update_settings: Arc::new(UpdateTenantSettings::new(
            tx_manager.clone(),
            settings,
            events.clone(),
        )),
        slug_availability: Arc::new(CheckSlugAvailability::new(tx_manager.clone(), slugs)),
        resolve,
        host_resolver,
        idempotency: IdempotencyStore::new(tx_manager.pool().clone()),
    }
}

struct UnreachableRepo;
#[async_trait::async_trait]
impl crate::domain::TenantRepository for UnreachableRepo {
    async fn save(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _tenant: &crate::domain::Tenant,
    ) -> Result<(), TenancyError> {
        unreachable!()
    }
    async fn find_by_id(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _id: TenantId,
    ) -> Result<Option<crate::domain::Tenant>, TenancyError> {
        unreachable!()
    }
    async fn find_by_slug(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _slug: &crate::domain::TenantSlug,
    ) -> Result<Option<crate::domain::Tenant>, TenancyError> {
        unreachable!()
    }
    async fn list(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _after: Option<TenantId>,
        _limit: u32,
        _status: Option<TenantStatus>,
    ) -> Result<(Vec<crate::domain::Tenant>, Option<TenantId>), TenancyError> {
        unreachable!()
    }
    async fn due_for_purge_request(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _before: chrono::DateTime<chrono::Utc>,
        _limit: u32,
    ) -> Result<Vec<crate::domain::Tenant>, TenancyError> {
        unreachable!()
    }
    async fn search(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _search: &crate::domain::TenantSearch,
    ) -> Result<
        (
            Vec<crate::domain::Tenant>,
            Option<crate::domain::TenantSearchCursor>,
        ),
        TenancyError,
    > {
        unreachable!()
    }
}

struct UnreachableSettings;
#[async_trait::async_trait]
impl crate::domain::TenantSettingsRepository for UnreachableSettings {
    async fn get(
        &self,
        _scope: &mut sanvi_persistence::ScopedTx<'_>,
        _tenant_id: TenantId,
        _key: &str,
    ) -> Result<Option<serde_json::Value>, TenancyError> {
        unreachable!()
    }
    async fn set(
        &self,
        _scope: &mut sanvi_persistence::ScopedTx<'_>,
        _tenant_id: TenantId,
        _key: &str,
        _value: serde_json::Value,
    ) -> Result<(), TenancyError> {
        unreachable!()
    }
    async fn list_all(
        &self,
        _scope: &mut sanvi_persistence::ScopedTx<'_>,
        _tenant_id: TenantId,
    ) -> Result<Vec<(String, serde_json::Value)>, TenancyError> {
        unreachable!()
    }
}

struct UnreachableSlugs;
#[async_trait::async_trait]
impl crate::domain::SlugReservationPort for UnreachableSlugs {
    async fn reserve(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _slug: &crate::domain::TenantSlug,
    ) -> Result<(), TenancyError> {
        unreachable!()
    }
    async fn is_available(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _slug: &crate::domain::TenantSlug,
    ) -> Result<bool, TenancyError> {
        unreachable!()
    }
}

struct UnreachableEvents;
#[async_trait::async_trait]
impl crate::domain::EventPublisher for UnreachableEvents {
    async fn publish(
        &self,
        _scope: &mut sanvi_persistence::PlatformScope<'_>,
        _events: &[crate::domain::TenantEvent],
    ) -> Result<(), TenancyError> {
        unreachable!()
    }
    async fn publish_scoped(
        &self,
        _scope: &mut sanvi_persistence::ScopedTx<'_>,
        _events: &[crate::domain::TenantEvent],
    ) -> Result<(), TenancyError> {
        unreachable!()
    }
    async fn publish_envelopes_scoped(
        &self,
        _scope: &mut sanvi_persistence::ScopedTx<'_>,
        _envelopes: &[sanvi_kernel::EventEnvelope],
    ) -> Result<(), TenancyError> {
        unreachable!()
    }
}

struct UnreachableIds;
impl crate::domain::IdGenerator for UnreachableIds {
    fn next_tenant_id(&self) -> Result<TenantId, TenancyError> {
        unreachable!()
    }
}

struct UnreachableClock;
impl crate::domain::Clock for UnreachableClock {
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        unreachable!()
    }
}

struct UnreachableCache;
#[async_trait::async_trait]
impl crate::domain::TenantResolutionCache for UnreachableCache {
    async fn get_slug(&self, _slug: &str) -> Result<Option<crate::domain::Tenant>, TenancyError> {
        unreachable!()
    }
    async fn get_id(&self, _id: TenantId) -> Result<Option<crate::domain::Tenant>, TenancyError> {
        unreachable!()
    }
    async fn put(&self, _tenant: &crate::domain::Tenant) -> Result<(), TenancyError> {
        unreachable!()
    }
    async fn invalidate(&self, _slug: &str, _id: TenantId) -> Result<(), TenancyError> {
        unreachable!()
    }
}
