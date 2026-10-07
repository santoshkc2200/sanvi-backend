//! Handlers: thin actix shims over the application layer.
//!
//! Domain errors map to [`ApiError`] at this boundary (the theming
//! context's slice of the single mapping rule). The public theme endpoint
//! resolves its tenant from the Host (a public path — no tenant
//! middleware); the preview path enforces its signed token and adds
//! `noindex`/`no-store`.

use actix_web::{HttpRequest, HttpResponse, web};
use sanvi_http::{
    ApiError, CurrentLocale, CurrentSubject, CurrentTenant, ProblemDetail, RequiredPermission,
};
use sanvi_persistence::{PlatformScope, ScopedTx};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::adapters::http::state::ThemingState;
use crate::application::UploadBrandAssetCommand;
use crate::application::draft::UpdateTenantThemeDraftCommand;
use crate::application::install::InstallThemeVersionCommand;
use crate::application::views::TenantThemeView;
use crate::domain::tenant_theme::{BrandAssetKind, ThemeState};
use crate::domain::{ResolvedTheme, ThemingError, sign_preview_token, verify_preview_token};

/// The theming context's domain-error → HTTP mapping.
pub fn api_error(err: ThemingError) -> ApiError {
    match err {
        ThemingError::UnknownTheme(detail) => ApiError::NotFound {
            resource: format!("theme {detail}"),
        },
        ThemingError::UnknownThemeVersion { theme, version } => ApiError::NotFound {
            resource: format!("theme version {theme}@{version}"),
        },
        ThemingError::ThemeVersionYanked { theme, version } => ApiError::Gone {
            detail: format!("theme version {theme}@{version} has been withdrawn"),
        },
        ThemingError::ThemeVersionExists { theme, version } => ApiError::Conflict {
            detail: format!("theme version {theme}@{version} already exists"),
        },
        ThemingError::ThemeDeprecated(detail) => ApiError::Gone {
            detail: format!("theme {detail} is deprecated"),
        },
        ThemingError::ThemeNotEntitled { feature } => ApiError::Forbidden {
            detail: format!("this theme requires the {feature} entitlement"),
        },
        ThemingError::ThemeApiMismatch { found, supported } => ApiError::BadRequest {
            detail: format!("theme api {found} is not compatible with {supported}"),
        },
        ThemingError::InvalidManifest(detail)
        | ThemingError::InvalidTokens(detail)
        | ThemingError::OverrideNotAllowed { path: detail }
        | ThemingError::OverrideTypeMismatch { path: detail, .. }
        | ThemingError::LayoutOverrideNotAllowed(detail)
        | ThemingError::InvalidCustomCss(detail)
        | ThemingError::InvalidBrandAsset(detail)
        | ThemingError::InvalidState(detail)
        | ThemingError::ContrastFailure(detail) => ApiError::BadRequest { detail },
        ThemingError::LayoutOverrideNotEntitled(detail)
        | ThemingError::CustomCssNotEntitled(detail) => ApiError::Forbidden { detail },
        ThemingError::NothingToRollBack => ApiError::Conflict {
            detail: "there is no previous revision to roll back to".to_string(),
        },
        ThemingError::InvalidPreviewToken => ApiError::Forbidden {
            detail: "invalid preview token".to_string(),
        },
        ThemingError::PreviewTokenExpired => ApiError::Forbidden {
            detail: "preview token expired".to_string(),
        },
        ThemingError::Storage(detail) => {
            tracing::error!(error = %detail, "theming storage failure");
            ApiError::from_storage_detail(&detail)
        }
        ThemingError::Cache(detail) => {
            tracing::error!(error = %detail, "theming cache failure");
            ApiError::internal("cache failure")
        }
        ThemingError::Outbox(detail) => {
            tracing::error!(error = %detail, "theming outbox failure");
            ApiError::internal("event publishing failure")
        }
        ThemingError::IdGeneration(detail) => {
            tracing::error!(error = %detail, "theming id generation failure");
            ApiError::internal("id generation failure")
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PublicThemeQuery {
    pub host: Option<String>,
    pub locale: Option<String>,
}

/// `GET /api/v1/public/theme` — the resolved live theme for the request
/// host (or the platform default), ETag-cached. No authentication; tenant
/// resolution happens here because public paths skip the tenant middleware.///
/// Cache semantics (TASK-022, FR-1111): when `platform.ssr_cache` is
/// enabled, the anonymous response is served from the server-side SSR cache
/// keyed by (tenant, locale, theme revision, device class) with
/// single-flight on miss; a theme publish or content change invalidates the
/// affected entries within ~2 s (the entry TTL is a backstop only).
/// Requests carrying an `Authorization` header are never served from or
/// written to the cache, and responses carrying `Set-Cookie` are never
/// stored. Varies on `Accept-Language`. The response preloads the tenant's
/// brand logo — the largest above-the-fold resource the backend knows
/// about — via a `Link; rel=preload` header.
#[utoipa::path(
    get,
    path = "/api/v1/public/theme",
    tag = "theming",
    operation_id = "get_public_theme",
    params(
        ("host" = Option<String>, Query, description = "Host to resolve (defaults to the request Host header)"),
        ("locale" = Option<String>, Query, description = "Requested locale (defaults to en)")
    ),
    responses(
        (status = 200, description = "Resolved theme with css_vars", body = crate::domain::ResolvedTheme),
        (status = 304, description = "Not modified (ETag match)"),
        (status = 400, description = "Invalid locale", body = ProblemDetail)
    )
)]
pub async fn get_public_theme(
    state: web::Data<ThemingState>,
    query: web::Query<PublicThemeQuery>,
    req: HttpRequest,
) -> Result<HttpResponse, ApiError> {
    let host = query
        .host
        .clone()
        .or_else(|| {
            req.headers()
                .get(actix_web::http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let locale = sanitize_locale(query.locale.as_deref().unwrap_or("en"))?;

    let resolved = state
        .get_public_theme
        .execute(&host, &locale)
        .await
        .map_err(api_error)?;

    if let Some(inbound) = req.headers().get(actix_web::http::header::IF_NONE_MATCH)
        && inbound
            .to_str()
            .map(|value| value == resolved.etag)
            .unwrap_or(false)
    {
        return Ok(HttpResponse::NotModified()
            .insert_header(("etag", resolved.etag.clone()))
            .insert_header(("cache-control", "public, no-cache"))
            .finish());
    }

    let mut response = HttpResponse::Ok();
    response
        .insert_header(("etag", resolved.etag.clone()))
        // The response may be retained by shared caches, but every reuse
        // must revalidate the ETag so a publish is visible immediately.
        .insert_header(("cache-control", "public, no-cache"));
    if let Some(link) = lcp_preload_hint(&resolved) {
        response.insert_header(("link", link));
    }
    Ok(response.json(resolved))
}

/// The LCP preload hint (TASK-022 11.3f, FR-1111): the tenant's brand logo
/// is the above-the-fold resource the backend knows about, so the theme
/// document response preloads it for the storefront's top templates. `None`
/// when the tenant ships no logo — a hint must never be invented.
pub fn lcp_preload_hint(resolved: &ResolvedTheme) -> Option<String> {
    resolved
        .brand_assets
        .logo
        .as_ref()
        .map(|logo| format!("<{logo}>; rel=preload; as=image"))
}

#[derive(Clone, Serialize, ToSchema)]
pub struct PreviewTokenView {
    pub token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// `POST /api/v1/tenant/theme/preview-token` — mint a short-lived token for
/// the current tenant's draft.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/theme/preview-token",
    tag = "theming",
    operation_id = "mint_theme_preview_token",
    responses(
        (status = 200, description = "Signed draft preview token", body = PreviewTokenView),
        (status = 500, description = "Preview signing is not configured", body = ProblemDetail)
    )
)]
pub async fn mint_theme_preview_token(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let secret = preview_secret(&state)?;
    let now = chrono::Utc::now();
    let ttl = state.config.preview_token_ttl_secs;
    let view = PreviewTokenView {
        token: sign_preview_token(&secret, tenant.0.tenant_id, ThemeState::Draft, ttl, now),
        expires_at: now + chrono::Duration::seconds(ttl as i64),
    };
    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store, private"))
        .json(view))
}

#[derive(Clone, Deserialize)]
pub struct PreviewThemeQuery {
    pub token: String,
    pub locale: Option<String>,
}

/// `GET /api/v1/tenant/theme/preview` — the draft, rendered through a
/// signed short-lived token. Never cached, never indexed.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/theme/preview",
    tag = "theming",
    operation_id = "preview_tenant_theme",
    params(
        ("token" = String, Query, description = "Signed preview token"),
        ("locale" = Option<String>, Query, description = "Requested locale (defaults to en)")
    ),
    responses(
        (status = 200, description = "Preview of the draft theme", body = crate::domain::ResolvedTheme),
        (status = 400, description = "Invalid locale", body = ProblemDetail),
        (status = 403, description = "Invalid or expired preview token", body = ProblemDetail)
    )
)]
pub async fn preview_tenant_theme(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    locale: CurrentLocale,
    query: web::Query<PreviewThemeQuery>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let secret = preview_secret(&state)?;
    let payload =
        verify_preview_token(&secret, &query.token, chrono::Utc::now()).map_err(api_error)?;
    if payload.tenant_id != tenant.0.tenant_id
        || ThemeState::parse(&payload.state).ok() != Some(ThemeState::Draft)
    {
        return Err(api_error(ThemingError::InvalidPreviewToken));
    }

    let locale = sanitize_locale(query.locale.as_deref().unwrap_or(&locale.0.locale))?;
    let resolved = state
        .preview_theme
        .execute(payload.tenant_id, &locale)
        .await
        .map_err(api_error)?;

    Ok(HttpResponse::Ok()
        .insert_header(("cache-control", "no-store, private"))
        .insert_header(("x-robots-tag", "noindex, nofollow"))
        .json(resolved))
}

fn preview_secret(state: &ThemingState) -> Result<Vec<u8>, ApiError> {
    if state.config.preview_token_secret.is_empty() {
        return Err(ApiError::internal(
            "preview signing is not configured on this deployment",
        ));
    }
    base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        state.config.preview_token_secret.expose_secret(),
    )
    .map_err(|err| {
        tracing::error!(error = %err, "preview token secret is not valid base64");
        ApiError::internal("preview signing is misconfigured")
    })
}

/// `GET /api/v1/tenant/themes/available`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/themes/available",
    tag = "theming",
    operation_id = "list_available_themes",
    responses(
        (status = 200, description = "Themes this tenant may choose from", body = crate::application::AvailableThemesView)
    )
)]
pub async fn list_available_themes(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let view = state
        .available
        .execute(tenant.0.tenant_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

/// `GET /api/v1/tenant/theme/draft`.
#[utoipa::path(
    get,
    path = "/api/v1/tenant/theme/draft",
    tag = "theming",
    operation_id = "get_tenant_theme_draft",
    responses(
        (status = 200, description = "The tenant's theme draft", body = crate::application::draft::TenantThemeDraftView)
    )
)]
pub async fn get_tenant_theme_draft(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let mut scope = begin_tenant_scope(&state.tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let view = state
        .get_draft
        .execute(&mut scope, tenant_id)
        .await
        .map_err(api_error)?;
    commit_scope(scope).await?;
    Ok(HttpResponse::Ok().json(view))
}

/// `PUT /api/v1/tenant/theme/draft`.
#[utoipa::path(
    put,
    path = "/api/v1/tenant/theme/draft",
    tag = "theming",
    operation_id = "put_tenant_theme_draft",
    request_body = UpdateTenantThemeDraftCommand,
    responses(
        (status = 200, description = "Updated draft", body = TenantThemeView),
        (status = 400, description = "Invalid draft", body = ProblemDetail),
        (status = 403, description = "Missing entitlement", body = ProblemDetail),
        (status = 404, description = "Unknown theme or version", body = ProblemDetail)
    )
)]
pub async fn put_tenant_theme_draft(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    locale: CurrentLocale,
    body: web::Json<UpdateTenantThemeDraftCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let mut scope = begin_tenant_scope(&state.tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .update_draft
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
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// `POST /api/v1/tenant/theme/publish`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/theme/publish",
    tag = "theming",
    operation_id = "publish_tenant_theme",
    responses(
        (status = 200, description = "The new live theme", body = TenantThemeView),
        (status = 400, description = "Draft failed validation (including contrast)", body = ProblemDetail),
        (status = 409, description = "No draft to publish", body = ProblemDetail)
    )
)]
pub async fn publish_tenant_theme(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    locale: CurrentLocale,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let mut scope = begin_tenant_scope(&state.tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .publish
        .execute(
            &mut scope,
            tenant_id,
            Some(subject.0.user_id),
            &locale.0.locale,
        )
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope(scope).await?;
            state.service.cache().invalidate_local(tenant_id);
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// `POST /api/v1/tenant/theme/rollback`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/theme/rollback",
    tag = "theming",
    operation_id = "rollback_tenant_theme",
    responses(
        (status = 200, description = "The restored live theme", body = TenantThemeView),
        (status = 409, description = "No previous revision", body = ProblemDetail)
    )
)]
pub async fn rollback_tenant_theme(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    locale: CurrentLocale,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let mut scope = begin_tenant_scope(&state.tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .rollback
        .execute(
            &mut scope,
            tenant_id,
            Some(subject.0.user_id),
            &locale.0.locale,
        )
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope(scope).await?;
            state.service.cache().invalidate_local(tenant_id);
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// `POST /api/v1/tenant/theme/assets`.
#[utoipa::path(
    post,
    path = "/api/v1/tenant/theme/assets",
    tag = "theming",
    operation_id = "upload_brand_asset",
    request_body = UploadBrandAssetCommand,
    responses(
        (status = 200, description = "Updated draft", body = TenantThemeView),
        (status = 400, description = "Invalid asset", body = ProblemDetail)
    )
)]
pub async fn upload_brand_asset(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    locale: CurrentLocale,
    body: web::Json<UploadBrandAssetCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let tenant_id = tenant.0.tenant_id;
    let mut scope = begin_tenant_scope(&state.tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .upload_asset
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
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// `DELETE /api/v1/tenant/theme/assets/{kind}`.
#[utoipa::path(
    delete,
    path = "/api/v1/tenant/theme/assets/{kind}",
    tag = "theming",
    operation_id = "remove_brand_asset",
    params(
        ("kind" = String, Path, description = "logo | favicon | og_image")
    ),
    responses(
        (status = 200, description = "Updated draft", body = TenantThemeView),
        (status = 400, description = "Unknown asset kind", body = ProblemDetail)
    )
)]
pub async fn remove_brand_asset(
    state: web::Data<ThemingState>,
    tenant: CurrentTenant,
    subject: CurrentSubject,
    locale: CurrentLocale,
    path: web::Path<String>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let kind = BrandAssetKind::parse(&path.into_inner()).map_err(api_error)?;
    let tenant_id = tenant.0.tenant_id;
    let mut scope = begin_tenant_scope(&state.tx_manager, tenant_id)
        .await
        .map_err(api_error)?;
    let result = state
        .remove_asset
        .execute(
            &mut scope,
            tenant_id,
            Some(subject.0.user_id),
            &locale.0.locale,
            kind,
        )
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope(scope).await?;
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// `GET /api/v1/platform/themes`.
#[utoipa::path(
    get,
    path = "/api/v1/platform/themes",
    tag = "theming",
    operation_id = "list_platform_themes",
    responses(
        (status = 200, description = "The full theme registry", body = Vec<crate::application::views::ThemeAdminView>)
    )
)]
pub async fn list_platform_themes(
    state: web::Data<ThemingState>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let view = state.list_themes.execute().await.map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

/// `POST /api/v1/platform/themes` — install a theme version.
#[utoipa::path(
    post,
    path = "/api/v1/platform/themes",
    tag = "theming",
    operation_id = "install_theme_version",
    request_body = InstallThemeVersionCommand,
    responses(
        (status = 200, description = "Installed version", body = crate::application::views::ThemeVersionAdminView),
        (status = 400, description = "Manifest, tokens or contrast validation failed", body = ProblemDetail),
        (status = 409, description = "Version already exists", body = ProblemDetail)
    )
)]
pub async fn install_theme_version(
    state: web::Data<ThemingState>,
    subject: CurrentSubject,
    body: web::Json<InstallThemeVersionCommand>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let actor = subject.0.user_id.to_string();
    let mut scope = begin_platform_scope(&state.tx_manager, "theming.theme.install", &actor)
        .await
        .map_err(api_error)?;
    let result = state
        .install_version
        .execute(&mut scope, &body.into_inner())
        .await
        .map_err(api_error);
    match result {
        Ok(view) => {
            commit_scope_platform(scope).await?;
            Ok(HttpResponse::Ok().json(view))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

#[derive(Debug, Clone, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ThemeVersionListQuery {
    /// Page size, clamped to 1..=200.
    #[param(schema_with = theme_version_limit_schema)]
    pub limit: Option<u32>,
    /// Cursor: the id of the last version of the previous page. Absent for
    /// the first page; the page is ordered newest first.
    pub after: Option<String>,
}

fn theme_version_limit_schema() -> utoipa::openapi::schema::Object {
    sanvi_http::openapi::page_limit_schema(200, 200).build()
}

/// `GET /api/v1/platform/themes/{key}/versions`.
#[utoipa::path(
    get,
    path = "/api/v1/platform/themes/{key}/versions",
    tag = "theming",
    operation_id = "list_platform_theme_versions",
    params(
        ("key" = String, Path, description = "Theme key"),
        ThemeVersionListQuery
    ),
    responses(
        (status = 200, description = "The theme's version history", body = Vec<crate::application::views::ThemeVersionAdminView>),
        (status = 400, description = "Malformed cursor", body = ProblemDetail),
        (status = 404, description = "Unknown theme", body = ProblemDetail),
        (status = 504, description = "Statement timeout - the request exceeded its budget", body = ProblemDetail)
    )
)]
pub async fn list_platform_theme_versions(
    state: web::Data<ThemingState>,
    path: web::Path<String>,
    query: web::Query<ThemeVersionListQuery>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let after_id = query
        .after
        .as_deref()
        .map(|raw| raw.parse::<i64>())
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let view = state
        .list_versions
        .execute(&path.into_inner(), query.limit, after_id)
        .await
        .map_err(api_error)?;
    Ok(HttpResponse::Ok().json(view))
}

/// `POST /api/v1/platform/themes/{key}/versions/{version}/yank` — withdraw.
#[utoipa::path(
    post,
    path = "/api/v1/platform/themes/{key}/versions/{version}/yank",
    tag = "theming",
    operation_id = "yank_theme_version",
    params(
        ("key" = String, Path, description = "Theme key"),
        ("version" = String, Path, description = "Semver to withdraw")
    ),
    responses(
        (status = 200, description = "Version withdrawn"),
        (status = 404, description = "Unknown theme or version", body = ProblemDetail)
    )
)]
pub async fn yank_theme_version(
    state: web::Data<ThemingState>,
    path: web::Path<(String, String)>,
    _guard: RequiredPermission,
) -> Result<HttpResponse, ApiError> {
    let (key, version) = path.into_inner();
    let mut scope = begin_platform_scope(&state.tx_manager, "theming.theme.yank", "theming")
        .await
        .map_err(api_error)?;
    let result = state
        .yank_version
        .execute(&mut scope, &key, &version)
        .await
        .map_err(api_error);
    match result {
        Ok(()) => {
            commit_scope_platform(scope).await?;
            Ok(HttpResponse::Ok().json(serde_json::json!({
                "theme": key,
                "version": version,
                "yanked": true
            })))
        }
        Err(err) => {
            let _ = scope.rollback().await;
            Err(err)
        }
    }
}

/// Locale guard: the resolution ETag includes the locale, so arbitrary
/// input would only fragment the cache — but it must still be sane.
fn sanitize_locale(raw: &str) -> Result<String, ApiError> {
    let trimmed = raw.trim().to_ascii_lowercase();
    let valid = (2..=12).contains(&trimmed.len())
        && trimmed.chars().all(|c| c.is_ascii_alphabetic() || c == '-');
    if valid {
        Ok(trimmed)
    } else {
        Err(ApiError::BadRequest {
            detail: format!("invalid locale {raw:?}"),
        })
    }
}

/// Tenant-scoped transaction helper.
async fn begin_tenant_scope<'a>(
    tx_manager: &'a sanvi_persistence::TxManager,
    tenant_id: sanvi_kernel::TenantId,
) -> Result<ScopedTx<'a>, ThemingError> {
    tx_manager
        .begin_for(tenant_id.as_i64())
        .await
        .map_err(|err| ThemingError::Storage(format!("failed to open tenant scope: {err}")))
}

/// Platform-scoped (audited) transaction helper.
async fn begin_platform_scope<'a>(
    tx_manager: &'a sanvi_persistence::TxManager,
    reason: &str,
    actor: &str,
) -> Result<PlatformScope<'a>, ThemingError> {
    tx_manager
        .begin_unscoped(reason, actor)
        .await
        .map_err(|err| ThemingError::Storage(format!("failed to open platform scope: {err}")))
}

async fn commit_scope(scope: ScopedTx<'_>) -> Result<(), ApiError> {
    scope.commit().await.map_err(|err| {
        tracing::error!(error = %err, "failed to commit theming transaction");
        ApiError::from_storage_detail(&err.to_string())
    })
}

async fn commit_scope_platform(scope: PlatformScope<'_>) -> Result<(), ApiError> {
    scope.commit().await.map_err(|err| {
        tracing::error!(error = %err, "failed to commit theming transaction");
        ApiError::from_storage_detail(&err.to_string())
    })
}

#[cfg(test)]
mod lcp_preload_tests {
    use super::*;
    use serde_json::json;

    fn resolved_theme(logo: Option<&str>) -> ResolvedTheme {
        let mut body = json!({
            "theme_key": "base",
            "theme_version": "1.0.0",
            "theme_api": "1",
            "capabilities": [],
            "tokens": {},
            "css_vars": ":root{}",
            "layouts": {},
            "fonts": [],
            "theme_assets": {"screenshots": []},
            "revision": 3,
            "locale": "en",
            "etag": "\"t\""
        });
        let assets = match logo {
            Some(url) => json!({"logo": url, "favicon": null, "og_image": null}),
            None => json!({"logo": null, "favicon": null, "og_image": null}),
        };
        body["brand_assets"] = assets;
        serde_json::from_value(body).expect("fixture deserializes")
    }

    /// The storefront's LCP preload rides the theme document: the tenant
    /// logo, as an image, in Link form (TASK-022 11.3f).
    #[test]
    fn the_brand_logo_is_preloaded_when_present() {
        let resolved = resolved_theme(Some(
            "https://cdn.example.com/assets/theming/logo-abc123.svg",
        ));
        assert_eq!(
            lcp_preload_hint(&resolved).as_deref(),
            Some("<https://cdn.example.com/assets/theming/logo-abc123.svg>; rel=preload; as=image")
        );
    }

    /// A tenant with no logo gets no hint: inventing a preload target is
    /// worse than none.
    #[test]
    fn no_logo_means_no_hint() {
        let resolved = resolved_theme(None);
        assert_eq!(lcp_preload_hint(&resolved), None);
    }
}
