//! Per-app response cache headers (TASK-022 11.3e, FR-1111).
//!
//! The backend ships one HTTP app; "per app" means its serving surfaces:
//! the platform-admin and tenant-console APIs are `no-store` — they render
//! session-scoped data — and the anonymous storefront reads carry a short
//! shared-cache lifetime, stamped only when the handler has not declared
//! its own `Cache-Control` (the theme endpoint's `public, no-cache`, for
//! one, stays authoritative). Theme dependence rides the request target
//! (the host *is* the tenant) and needs no header; locale dependence rides
//! `Accept-Language`, stamped by the locale middleware and pinned by the
//! cache-headers snapshot in `apps/api`.

use std::sync::Arc;

use actix_web::{
    body::BoxBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::{
        Method,
        header::{self, HeaderValue},
    },
};

use crate::config::CacheHeadersConfig;

/// The admin surfaces: every response — any method, any status — is
/// `no-store`, overriding whatever a handler set. Session-scoped data must
/// never sit in a browser or intermediary cache.
pub const ADMIN_CACHE_PREFIXES: &[&str] = &["/api/v1/platform", "/api/v1/tenant"];

/// The anonymous storefront reads: 2xx GET/HEAD responses without their own
/// `Cache-Control` gain the configured short shared-cache lifetime.
pub const STOREFRONT_CACHE_PREFIXES: &[&str] = &["/api/v1/public/"];

const NO_STORE: &str = "no-store";

#[derive(Clone)]
pub struct CacheHeadersLayer {
    config: CacheHeadersConfig,
}

impl CacheHeadersLayer {
    pub fn new(config: CacheHeadersConfig) -> Self {
        Self { config }
    }
}

impl<S> Transform<S, ServiceRequest> for CacheHeadersLayer
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = CacheHeadersService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(CacheHeadersService {
            service,
            config: Arc::new(self.config.clone()),
        }))
    }
}

pub struct CacheHeadersService<S> {
    service: S,
    config: Arc<CacheHeadersConfig>,
}

impl<S> Service<ServiceRequest> for CacheHeadersService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let admin = ADMIN_CACHE_PREFIXES
            .iter()
            .any(|prefix| req.path().starts_with(prefix));
        let storefront_fill = !admin
            && matches!(*req.method(), Method::GET | Method::HEAD)
            && STOREFRONT_CACHE_PREFIXES
                .iter()
                .any(|prefix| req.path().starts_with(prefix));
        let max_age = self.config.storefront_max_age_secs;

        let fut = self.service.call(req);
        Box::pin(async move {
            let mut res = fut.await?;
            if admin {
                res.response_mut()
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
                return Ok(res);
            }
            if storefront_fill
                && res.status().is_success()
                && !res.headers().contains_key(header::CACHE_CONTROL)
            {
                let value = HeaderValue::from_str(&format!("public, max-age={max_age}"))
                    .expect("the storefront max-age renders to a valid header value");
                res.response_mut()
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, value);
            }
            Ok(res)
        })
    }
}
