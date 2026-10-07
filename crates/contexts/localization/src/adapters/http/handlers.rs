//! Handlers: thin actix shims over the application layer.
//!
//! Domain errors map to [`ApiError`] at this boundary (the localization
//! context's slice of the single mapping rule); every handler-originated
//! string uses the `t!` catalog.

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentLocale, CurrentSubject, CurrentTenant, ProblemDetail, RequiredPermission,
};
use sanvi_kernel::TenantId;
use sanvi_persistence::{PlatformScope, ScopedTx};
use serde::{Deserialize, Serialize};

use crate::adapters::http::state::LocalizationState;
use crate::application::{TranslationPatch, UpdateLocalizationSettingsCommand};
use crate::domain::{Locale, LocalizationError};

/// The localization context's domain-error → HTTP mapping.
pub fn api_error(err: LocalizationError) -> ApiError {
    match err {
        LocalizationError::InvalidLocale(detail)
        | LocalizationError::InvalidTimezone(detail)
        | LocalizationError::InvalidOverride { detail, .. }
        | LocalizationError::InvalidAddress(detail) => ApiError::BadRequest { detail },
        LocalizationError::UnsupportedLocale(detail) => ApiError::BadRequest {
            detail: format!("locale '{detail}' is not supported on this platform"),
        },
        LocalizationError::UnknownDomain(detail) => ApiError::NotFound {
            resource: format!("message domain {detail}"),
        },
        LocalizationError::UnknownMessageKey(detail) => ApiError::BadRequest {
            detail: format!("unknown message key '{detail}'"),
        },
        LocalizationError::SettingsConflict(detail) => ApiError::Conflict { detail },
        LocalizationError::InvalidPattern { key, detail } => ApiError::BadRequest {
            detail: format!("invalid message pattern for '{key}': {detail}"),
        },
        LocalizationError::UnknownCurrency(detail) => {
            ApiError::internal(format!("currency {detail} is not in the exponent table"))
        }
        LocalizationError::Storage(detail) => {
            tracing::error!(error = %detail, "localization storage failure");
            ApiError::from_storage_detail(&detail)
        }
        LocalizationError::Outbox(detail) => {
            tracing::error!(error = %detail, "localization outbox failure");
            ApiError::internal("event publishing failure")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SupportedLocalesView {
    pub default: String,
    pub locales: Vec<crate::domain::SupportedLocale>,
}

/// `GET /api/v1/public/locales` — the platform's enabled locale set.///
/// Cache semantics (TASK-022, FR-1111): when `platform.ssr_cache` is
/// enabled, the anonymous response is served from the server-side SSR cache
/// keyed by (tenant, locale, theme revision, device class) with
/// single-flight on miss; a theme publish or content change invalidates the
/// affected entries within ~2 s (the entry TTL is a backstop only).
/// Requests carrying an `Authorization` header are never served from or
/// written to the cache, and responses carrying `Set-Cookie` are never
/// stored. Varies on `Accept-Language`.
#[utoipa::path(
    get,
    path = "/api/v1/public/locales",
    tag = "localization",
    operation_id = "get_public_locales",
    responses(
        (status = 200, description = "Supported locales", body = SupportedLocalesView)
    )
)]
pub async fn get_public_locales(
    state: web::Data<LocalizationState>,
) -> Result<HttpResponse, ApiError> {
    let locales = state.catalog.supported_locales().await.map_err(api_error)?;
    Ok(HttpResponse::Ok().json(SupportedLocalesView {
        default: state.catalog.default_locale().to_string(),
        locales,
    }))
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessagesQuery {
    pub locale: Option<String>,
}

/// `GET /api/v1/public/messages/{domain}?locale=` — catalog delivery for
/// the frontend, ETag-cached.///
/// Cache semantics (TASK-022, FR-1111): when `platform.ssr_cache` is
/// enabled, the anonymous response is served from the server-side SSR cache
/// keyed by (tenant, locale, theme revision, device class) with
/// single-flight on miss; a theme publish or content change invalidates the
/// affected entries within ~2 s (the entry TTL is a backstop only).
/// Requests carrying an `Authorization` header are never served from or
/// written to the cache, and responses carrying `Set-Cookie` are never
/// stored. Varies on `Accept-Language`.
#[utoipa::path(
    get,
    path = "/api/v1/public/messages/{domain}",
    tag = "localization",
    operation_id = "get_public_messages",
    params(
        ("domain" = String, Path, description = "Catalog domain (billing, emails, errors, privacy)"),
        ("locale" = Option<String>, Query, description = "Requested locale (defaults to en)")
    ),
    responses(
        (status = 200, description = "Message catalog", body = crate::application::CatalogView),
        (status = 304, description = "Not modified (ETag match)"),
        (status = 404, description = "Unknown domain", body = ProblemDetail)
    )
)]
pub async fn get_public_messages(
    state: web::Data<LocalizationState>,
    path: web::Path<String>,
    query: web::Query<MessagesQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, ApiError> {
    let domain = path.into_inner();
    let locale = query
        .locale
        .clone()
        .unwrap_or_else(|| state.catalog.default_locale().to_string());
    let locale = Locale::parse(&locale).map_err(api_error)?;

    let view = state
        .catalog
        .catalog("platform", &domain, locale.as_str())
        .await
        .map_err(api_error)?;

    let etag = format!("\"{}\"", view.revision);
    if let Some(inbound) = req.headers().get(actix_web::http::header::IF_NONE_MATCH)
        && inbound.to_str().map(|value| value == etag).unwrap_or(false)
    {
        return Ok(HttpResponse::NotModified().finish());
    }

    Ok(HttpResponse::Ok()
        .insert_header(("etag", etag))
        .insert_header(("content-language", locale.as_str()))
        .json(view))
}

/// `GET /api/v1/tenant/localization/settings`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/localization/settings",
    tag = "localization",
    operation_id = "get_localization_settings",
    responses(
        (status = 200, description = "Tenant localization settings", body = crate::application::LocalizationSettingsView),
        (status = 404, description = "Tenant not found", body = ProblemDetail)
    )
)]
pub async fn get_localization_settings(
    state: web::Data<LocalizationState>,
    tenant: CurrentTenant,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let tx_manager = sanvi_persistence::TxManager::new(state.pool.clone(), state.ids.clone());
    let mut scope = begin_tenant_scope(&tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .get_settings
        .execute(&mut scope, tenant_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

/// `PUT /api/v1/tenant/localization/settings`.
#[utoipa::path(
    put,
    path = "/api/v1/tenant/localization/settings",
    tag = "localization",
    operation_id = "put_localization_settings",
    request_body = UpdateLocalizationSettingsCommand,
    responses(
        (status = 200, description = "Updated settings", body = crate::application::LocalizationSettingsView),
        (status = 400, description = "Invalid settings", body = ProblemDetail),
        (status = 409, description = "Settings conflict", body = ProblemDetail)
    )
)]
pub async fn put_localization_settings(
    state: web::Data<LocalizationState>,
    tenant: CurrentTenant,
    locale: CurrentLocale,
    body: web::Json<UpdateLocalizationSettingsCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let mut command = body.into_inner();
    if command.acting_locale.is_empty() {
        command.acting_locale = locale.0.locale.clone();
    }
    let tx_manager = sanvi_persistence::TxManager::new(state.pool.clone(), state.ids.clone());
    let mut scope = begin_tenant_scope(&tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .update_settings
        .execute(&mut scope, tenant_id, &command)
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope(scope).await?;
            state.service.invalidate_tenant(tenant_id);
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// `GET /api/v1/tenant/localization/overrides`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/localization/overrides",
    tag = "localization",
    operation_id = "get_localization_overrides",
    responses(
        (status = 200, description = "Tenant copy overrides", body = Vec<crate::application::TranslationView>)
    )
)]
pub async fn get_localization_overrides(
    state: web::Data<LocalizationState>,
    tenant: CurrentTenant,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let tx_manager = sanvi_persistence::TxManager::new(state.pool.clone(), state.ids.clone());
    let mut scope = begin_tenant_scope(&tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .get_overrides
        .execute(&mut scope, tenant_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

/// `PUT /api/v1/tenant/localization/overrides`.
#[utoipa::path(
    put,
    path = "/api/v1/tenant/localization/overrides",
    tag = "localization",
    operation_id = "put_localization_overrides",
    request_body = Vec<TranslationPatch>,
    responses(
        (status = 200, description = "Updated overrides", body = Vec<crate::application::TranslationView>),
        (status = 400, description = "Invalid override", body = ProblemDetail)
    )
)]
pub async fn put_localization_overrides(
    state: web::Data<LocalizationState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    locale: CurrentLocale,
    body: web::Json<Vec<TranslationPatch>>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let tx_manager = sanvi_persistence::TxManager::new(state.pool.clone(), state.ids.clone());
    let mut scope = begin_tenant_scope(&tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .update_overrides
        .execute(
            &mut scope,
            tenant_id,
            Some(subject.0.user_id),
            &locale.0.locale,
            &body.into_inner(),
        )
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope(scope).await?;
            state.service.invalidate_tenant(tenant_id);
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// `GET /api/v1/platform/localization/translations`.
#[utoipa::path(
    get,
    path = "/api/v1/platform/localization/translations",
    tag = "localization",
    operation_id = "get_platform_translations",
    responses(
        (status = 200, description = "Platform catalog overrides", body = Vec<crate::application::TranslationView>)
    )
)]
pub async fn get_platform_translations(
    state: web::Data<LocalizationState>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let view = state
        .get_platform_translations
        .execute()
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

/// `PUT /api/v1/platform/localization/translations`.
#[utoipa::path(
    put,
    path = "/api/v1/platform/localization/translations",
    tag = "localization",
    operation_id = "put_platform_translations",
    request_body = Vec<TranslationPatch>,
    responses(
        (status = 200, description = "Updated platform overrides", body = Vec<crate::application::TranslationView>),
        (status = 400, description = "Invalid override", body = ProblemDetail)
    )
)]
pub async fn put_platform_translations(
    state: web::Data<LocalizationState>,
    subject: CurrentSubject,
    locale: CurrentLocale,
    body: web::Json<Vec<TranslationPatch>>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tx_manager = sanvi_persistence::TxManager::new(state.pool.clone(), state.ids.clone());
    let mut scope = begin_platform_scope(
        &tx_manager,
        "localization.translations.update",
        "localization",
    )
    .await
    .map_err(api_error)?;
    let result = state
        .update_platform_translations
        .execute(
            &mut scope,
            Some(subject.0.user_id),
            &locale.0.locale,
            &body.into_inner(),
        )
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope_platform(scope).await?;
            state.service.invalidate_scope("platform");
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// Tenant-scoped transaction helper: the scope borrows the transaction
/// manager, which must live on the caller's stack.
async fn begin_tenant_scope<'a>(
    tx_manager: &'a sanvi_persistence::TxManager,
    tenant_id: TenantId,
) -> Result<sanvi_persistence::ScopedTx<'a>, LocalizationError> {
    tx_manager
        .begin_for(tenant_id.as_i64())
        .await
        .map_err(|err| LocalizationError::Storage(format!("failed to open tenant scope: {err}")))
}

/// Platform-scoped (audited) transaction helper.
async fn begin_platform_scope<'a>(
    tx_manager: &'a sanvi_persistence::TxManager,
    reason: &str,
    actor: &str,
) -> Result<sanvi_persistence::PlatformScope<'a>, LocalizationError> {
    tx_manager
        .begin_unscoped(reason, actor)
        .await
        .map_err(|err| LocalizationError::Storage(format!("failed to open platform scope: {err}")))
}

async fn commit_scope(scope: ScopedTx<'_>) -> Result<(), ApiError> {
    scope.commit().await.map_err(|err| {
        tracing::error!(error = %err, "failed to commit localization transaction");
        ApiError::from_storage_detail(&err.to_string())
    })
}

async fn commit_scope_platform(scope: PlatformScope<'_>) -> Result<(), ApiError> {
    scope.commit().await.map_err(|err| {
        tracing::error!(error = %err, "failed to commit localization transaction");
        ApiError::from_storage_detail(&err.to_string())
    })
}
