//! Route registration for the theming context.
//!
//! Permission declarations ride on each resource's `app_data` (read back by
//! the `RequiredPermission` extractor) and are mirrored in
//! [`PERMISSIONED_ROUTES`] for the "no route without a permission" CI test.

use actix_web::http::Method;
use actix_web::web;
use sanvi_http::{ResourceSpec, require_permission};

use super::handlers::{
    get_public_theme, get_tenant_theme_draft, install_theme_version, list_available_themes,
    list_platform_theme_versions, list_platform_themes, mint_theme_preview_token,
    preview_tenant_theme, publish_tenant_theme, put_tenant_theme_draft, remove_brand_asset,
    rollback_tenant_theme, upload_brand_asset, yank_theme_version,
};

/// Public routes: no tenant or authentication required. The theme endpoint
/// resolves its tenant from the Host itself (public paths skip the tenant
/// middleware).
pub fn theming_public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(web::scope("/api/v1/public").route("/theme", web::get().to(get_public_theme)));
}

/// Tenant-scoped theme control (mounted inside the shared `/api/v1/tenant`
/// scope by the composition root).
pub fn theming_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    theming_tenant_scope_routes_with_asset_limit(cfg, crate::domain::MAX_ASSET_BYTES);
}

/// Register tenant routes with a JSON limit large enough to carry a base64
/// encoding of the configured binary asset cap.
pub fn theming_tenant_scope_routes_with_asset_limit(
    cfg: &mut web::ServiceConfig,
    asset_max_bytes: usize,
) {
    let draft_map = sanvi_http::PermissionMap::new(vec![
        (
            Method::GET,
            require_permission!("tenant.theming.read", tenant),
        ),
        (
            Method::PUT,
            require_permission!("tenant.theming.write", tenant),
        ),
    ]);
    cfg.service(
        web::resource("/theme/draft")
            .app_data(draft_map)
            .route(web::get().to(get_tenant_theme_draft))
            .route(web::put().to(put_tenant_theme_draft)),
    )
    .service(
        web::resource("/theme/publish")
            .app_data(require_permission!("tenant.theming.publish", tenant))
            .route(web::post().to(publish_tenant_theme)),
    )
    .service(
        web::resource("/theme/rollback")
            .app_data(require_permission!("tenant.theming.publish", tenant))
            .route(web::post().to(rollback_tenant_theme)),
    )
    .service(
        web::resource("/theme/preview")
            .app_data(require_permission!("tenant.theming.read", tenant))
            .route(web::get().to(preview_tenant_theme)),
    )
    .service(
        web::resource("/theme/preview-token")
            .app_data(require_permission!("tenant.theming.read", tenant))
            .route(web::post().to(mint_theme_preview_token)),
    )
    .service(
        web::resource("/themes/available")
            .app_data(require_permission!("tenant.theming.read", tenant))
            .route(web::get().to(list_available_themes)),
    )
    .service(
        web::resource("/theme/assets")
            .app_data(require_permission!("tenant.theming.write", tenant))
            .app_data(web::JsonConfig::default().limit(asset_json_limit(asset_max_bytes)))
            .route(web::post().to(upload_brand_asset)),
    )
    .service(
        web::resource("/theme/assets/{kind}")
            .app_data(require_permission!("tenant.theming.write", tenant))
            .route(web::delete().to(remove_brand_asset)),
    );
}

fn asset_json_limit(asset_max_bytes: usize) -> usize {
    // Base64 expands by ceil(n / 3) * 4; leave bounded room for JSON field
    // names, the declared MIME type and escaping overhead.
    asset_max_bytes
        .saturating_add(2)
        .saturating_div(3)
        .saturating_mul(4)
        .saturating_add(4 * 1024)
}

/// Platform-scoped theme registry administration.
pub fn theming_platform_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("platform.theme.read", platform);
    let write = require_permission!("platform.theme.write", platform);
    cfg.service(
        web::resource("/api/v1/platform/themes")
            .app_data(sanvi_http::PermissionMap::new(vec![
                (Method::GET, read),
                (Method::POST, write),
            ]))
            .route(web::get().to(list_platform_themes))
            .route(web::post().to(install_theme_version)),
    )
    .service(
        web::resource("/api/v1/platform/themes/{key}/versions")
            .app_data(read)
            .route(web::get().to(list_platform_theme_versions)),
    )
    .service(
        web::resource("/api/v1/platform/themes/{key}/versions/{version}/yank")
            .app_data(write)
            .route(web::post().to(yank_theme_version)),
    );
}

/// The non-public theming routes with their permission requirements —
/// enumerated by the permission catalog CI test.
pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/tenant/theme/draft",
        Method::GET,
        "tenant.theming.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/theme/draft",
        Method::PUT,
        "tenant.theming.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/theme/publish",
        Method::POST,
        "tenant.theming.publish",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/theme/rollback",
        Method::POST,
        "tenant.theming.publish",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/theme/preview",
        Method::GET,
        "tenant.theming.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/theme/preview-token",
        Method::POST,
        "tenant.theming.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/themes/available",
        Method::GET,
        "tenant.theming.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/theme/assets",
        Method::POST,
        "tenant.theming.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/theme/assets/{kind}",
        Method::DELETE,
        "tenant.theming.write",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/platform/themes",
        Method::GET,
        "platform.theme.read",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/themes",
        Method::POST,
        "platform.theme.write",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/themes/{key}/versions",
        Method::GET,
        "platform.theme.read",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/themes/{key}/versions/{version}/yank",
        Method::POST,
        "platform.theme.write",
        ResourceSpec::Platform,
    ),
];

#[cfg(test)]
mod tests {
    use super::asset_json_limit;

    #[test]
    fn asset_json_limit_accounts_for_base64_expansion() {
        let binary_cap = 2 * 1024 * 1024;
        assert!(asset_json_limit(binary_cap) > binary_cap);
        assert!(asset_json_limit(binary_cap) >= binary_cap.div_ceil(3) * 4);
    }
}
