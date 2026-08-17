//! Route registration for the localization context.
//!
//! Permission declarations ride on each resource's `app_data` (read back by
//! the `RequiredPermission` extractor) and are mirrored in
//! [`PERMISSIONED_ROUTES`] for the "no route without a permission" CI test.

use actix_web::http::Method;
use actix_web::web;
use sanvi_http::{ResourceSpec, require_permission};

use super::handlers::{
    get_localization_overrides, get_localization_settings, get_platform_translations,
    get_public_locales, get_public_messages, put_localization_overrides, put_localization_settings,
    put_platform_translations,
};

/// Public routes: no tenant or authentication required. The messages
/// endpoint serves the platform catalogs (static + platform overrides);
/// tenant overrides merge inside tenant-scoped requests.
pub fn localization_public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1/public")
            .route("/locales", web::get().to(get_public_locales))
            .route("/messages/{domain}", web::get().to(get_public_messages)),
    );
}

/// Tenant-scoped localization settings and copy overrides.
pub fn localization_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let settings_map = sanvi_http::PermissionMap::new(vec![
        (
            Method::GET,
            require_permission!("tenant.localization.settings.read", tenant),
        ),
        (
            Method::PUT,
            require_permission!("tenant.localization.settings.update", tenant),
        ),
    ]);
    let overrides_map = sanvi_http::PermissionMap::new(vec![
        (
            Method::GET,
            require_permission!("tenant.localization.overrides.read", tenant),
        ),
        (
            Method::PUT,
            require_permission!("tenant.localization.overrides.update", tenant),
        ),
    ]);
    cfg.service(
        web::resource("/localization/settings")
            .app_data(settings_map)
            .route(web::get().to(get_localization_settings))
            .route(web::put().to(put_localization_settings)),
    )
    .service(
        web::resource("/localization/overrides")
            .app_data(overrides_map)
            .route(web::get().to(get_localization_overrides))
            .route(web::put().to(put_localization_overrides)),
    );
}

/// Platform-scoped catalog override administration.
pub fn localization_platform_routes(cfg: &mut web::ServiceConfig) {
    let manage = require_permission!("platform.localization.translations.write", platform);
    let read = require_permission!("platform.localization.translations.read", platform);
    cfg.service(
        web::resource("/api/v1/platform/localization/translations")
            .app_data(sanvi_http::PermissionMap::new(vec![
                (Method::GET, read),
                (Method::PUT, manage),
            ]))
            .route(web::get().to(get_platform_translations))
            .route(web::put().to(put_platform_translations)),
    );
}

/// The non-public localization routes with their permission requirements —
/// enumerated by the permission catalog CI test.
pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/tenant/localization/settings",
        Method::GET,
        "tenant.localization.settings.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/localization/settings",
        Method::PUT,
        "tenant.localization.settings.update",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/localization/overrides",
        Method::GET,
        "tenant.localization.overrides.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/localization/overrides",
        Method::PUT,
        "tenant.localization.overrides.update",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/platform/localization/translations",
        Method::GET,
        "platform.localization.translations.read",
        ResourceSpec::Platform,
    ),
    (
        "/api/v1/platform/localization/translations",
        Method::PUT,
        "platform.localization.translations.write",
        ResourceSpec::Platform,
    ),
];
