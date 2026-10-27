//! Mounts inside the shared `/api/v1/tenant` scope, so tenant resolution,
//! authentication, CORS and rate limits apply before the proxy runs.
//!
//! Only the methods ecommerce-service serves are proxied (GET, POST); add
//! the next one alongside the first service route that needs it.
//!
//! No per-route permission is declared for ecommerce: the product service authorizes each
//! use case from the delegated `ecommerce.*` permissions. The proxy itself
//! refuses users holding none of them and tenants without the product.
//!
//! Connector proxy routes (`ecommerce-imports`) declare permissions explicitly
//! via [`PermissionMap`].

use actix_web::http::Method;
use actix_web::web;
use sanvi_http::{PermissionMap, ResourceSpec, require_permission};

use super::handlers::{
    proxy_ecommerce_get, proxy_ecommerce_post, proxy_import_callback, proxy_import_delete,
    proxy_import_get, proxy_import_post,
};

pub fn product_access_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("ecommerce.import.read", tenant);
    let manage = require_permission!("ecommerce.import.manage", tenant);

    cfg.service(
        web::resource("/ecommerce/{tail:.+}")
            .route(web::get().to(proxy_ecommerce_get))
            .route(web::post().to(proxy_ecommerce_post)),
    );
    cfg.service(
        web::resource("/ecommerce-imports/stores/{store_id}/connections/{provider}/callback")
            .app_data(PermissionMap::new(vec![(Method::GET, manage)]))
            .route(web::get().to(proxy_import_callback)),
    );
    cfg.service(
        web::resource("/ecommerce-imports/stores/{store_id}/{tail:.+}")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::POST, manage),
                (Method::DELETE, manage),
            ]))
            .route(web::get().to(proxy_import_get))
            .route(web::post().to(proxy_import_post))
            .route(web::delete().to(proxy_import_delete)),
    );
}

pub const PERMISSIONED_ROUTES: &[(&str, Method, &str, ResourceSpec)] = &[
    (
        "/api/v1/tenant/ecommerce-imports/stores/{store_id}/connections/{provider}/callback",
        Method::GET,
        "ecommerce.import.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}",
        Method::GET,
        "ecommerce.import.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}",
        Method::POST,
        "ecommerce.import.manage",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ecommerce-imports/stores/{store_id}/{tail}",
        Method::DELETE,
        "ecommerce.import.manage",
        ResourceSpec::Tenant,
    ),
];
