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
use super::shopper_handlers::{
    get_public_order, get_public_payment, get_public_product, get_public_products,
    get_public_store, get_published_store, post_public_checkout, post_public_checkout_release,
    post_public_order, post_public_order_cancel, post_public_payment, post_public_payment_cancel,
    put_published_store,
};

/// Public shopper routes (phase 14): full-path resources, no tenant or
/// authentication middleware — every handler resolves the tenant from the
/// validated host itself. Unconfigured upstreams fail closed with 503.
pub fn product_access_public_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/v1/public/ecommerce/store").route(web::get().to(get_public_store)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/products")
            .route(web::get().to(get_public_products)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/products/{product_id}")
            .route(web::get().to(get_public_product)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/checkout")
            .route(web::post().to(post_public_checkout)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/checkout/release")
            .route(web::post().to(post_public_checkout_release)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/orders").route(web::post().to(post_public_order)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/orders/{order_id}")
            .route(web::get().to(get_public_order)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/orders/{order_id}/cancel")
            .route(web::post().to(post_public_order_cancel)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/orders/{order_id}/payment")
            .route(web::post().to(post_public_payment))
            .route(web::get().to(get_public_payment)),
    )
    .service(
        web::resource("/api/v1/public/ecommerce/orders/{order_id}/payment/cancel")
            .route(web::post().to(post_public_payment_cancel)),
    );
}

/// Tenant-side administration of the published-store binding (staff only;
/// the routes declare their permissions explicitly via `PermissionMap`).
pub fn product_access_binding_routes(cfg: &mut web::ServiceConfig) {
    let read = require_permission!("ecommerce.store.read", tenant);
    let manage = require_permission!("ecommerce.store.manage", tenant);
    cfg.service(
        web::resource("/api/v1/tenant/ecommerce-public-store")
            .app_data(PermissionMap::new(vec![
                (Method::GET, read),
                (Method::PUT, manage),
            ]))
            .route(web::get().to(get_published_store))
            .route(web::put().to(put_published_store)),
    );
}

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
        "/api/v1/tenant/ecommerce-public-store",
        Method::GET,
        "ecommerce.store.read",
        ResourceSpec::Tenant,
    ),
    (
        "/api/v1/tenant/ecommerce-public-store",
        Method::PUT,
        "ecommerce.store.manage",
        ResourceSpec::Tenant,
    ),
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
