//! Mounts inside the shared `/api/v1/tenant` scope, so tenant resolution,
//! authentication, CORS and rate limits apply before the proxy runs.
//!
//! Only the methods ecommerce-service serves are proxied (GET, POST); add
//! the next one alongside the first service route that needs it.
//!
//! No per-route permission is declared: the product service authorizes each
//! use case from the delegated `ecommerce.*` permissions. The proxy itself
//! refuses users holding none of them and tenants without the product.

use actix_web::web;

use super::handlers::{proxy_ecommerce_get, proxy_ecommerce_post};

pub fn product_access_tenant_scope_routes(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/ecommerce/{tail:.+}")
            .route(web::get().to(proxy_ecommerce_get))
            .route(web::post().to(proxy_ecommerce_post)),
    );
}
