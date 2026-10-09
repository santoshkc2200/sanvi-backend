//! The degraded-response header (TASK-023, FR-1113): responses served while
//! a dependency runs degraded carry `X-Sanvi-Degraded` so the frontend can
//! say so honestly instead of pretending everything is fine.

use std::sync::Arc;

use actix_web::{
    body::BoxBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    http::header::{HeaderName, HeaderValue},
};

use sanvi_resilience::DegradationRegistry;

/// The documented header name. The value is the comma-separated list of
/// active degradation slugs (`redis`, `keto`, `stripe`, `ads`,
/// `object_storage`, `postgres`).
pub const DEGRADED_HEADER: &str = "x-sanvi-degraded";

/// Mountable layer stamping [`DEGRADED_HEADER`] on every response the app
/// produces while any degradation reason is active. Sits outermost in the
/// chain (before the error handlers, which rebuild error bodies) so a shed
/// `503` and a dependency-failed `5xx` carry it exactly like a healthy
/// `200`. Built once per process by the composition root: the flag-on
/// assembly passes the shared [`DegradationRegistry`], the flag-off
/// assembly builds [`DegradedHeaderLayer::disabled`], a pass-through.
#[derive(Clone, Default)]
pub struct DegradedHeaderLayer {
    registry: Option<Arc<DegradationRegistry>>,
}

impl DegradedHeaderLayer {
    /// The `platform.degraded_mode` flag-on assembly.
    pub fn new(registry: Arc<DegradationRegistry>) -> Self {
        Self {
            registry: Some(registry),
        }
    }

    /// The flag-off assembly: no header, whatever the registry says.
    pub fn disabled() -> Self {
        Self { registry: None }
    }
}

impl<S> Transform<S, ServiceRequest> for DegradedHeaderLayer
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = DegradedHeaderService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(DegradedHeaderService {
            service,
            registry: self.registry.clone(),
        }))
    }
}

pub struct DegradedHeaderService<S> {
    service: S,
    registry: Option<Arc<DegradationRegistry>>,
}

#[derive(Debug, thiserror::Error)]
#[error("{inner}")]
struct DegradedError {
    inner: actix_web::Error,
    header: HeaderValue,
}

impl actix_web::ResponseError for DegradedError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        self.inner.as_response_error().status_code()
    }
    fn error_response(&self) -> actix_web::HttpResponse {
        let mut response = self.inner.error_response();
        response.headers_mut().insert(
            HeaderName::from_static(DEGRADED_HEADER),
            self.header.clone(),
        );
        response
    }
}

impl<S> Service<ServiceRequest> for DegradedHeaderService<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let registry = self.registry.clone();
        let fut = self.service.call(req);
        Box::pin(async move {
            // Actix renders middleware errors outside this stack. Wrap
            // their renderer; an early request clone would prevent routing.
            let mut res = match fut.await {
                Ok(res) => res,
                Err(err) => {
                    if let Some(value) = registry.as_ref().and_then(|r| r.header_value())
                        && let Ok(header) = HeaderValue::from_str(&value)
                    {
                        return Err(DegradedError { inner: err, header }.into());
                    }
                    return Err(err);
                }
            };
            if let Some(registry) = registry.as_ref()
                && let Some(value) = registry.header_value()
                && let Ok(header_value) = HeaderValue::from_str(&value)
            {
                res.response_mut()
                    .headers_mut()
                    .insert(HeaderName::from_static(DEGRADED_HEADER), header_value);
            }
            Ok(res)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpResponse, web};

    async fn served_status_and_header(layer: DegradedHeaderLayer) -> (u16, Option<String>) {
        let app = actix_web::test::init_service(App::new().wrap(layer).service(
            web::resource("/").route(web::get().to(|| async { HttpResponse::Ok().body("ok") })),
        ))
        .await;
        let request = actix_web::test::TestRequest::default().to_request();
        let response = actix_web::test::call_service(&app, request).await;
        let header = response
            .headers()
            .get(DEGRADED_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        (response.status().as_u16(), header)
    }

    #[tokio::test]
    async fn stamps_the_header_only_while_a_reason_is_active() {
        let registry = Arc::new(DegradationRegistry::default());
        let (status, header) =
            served_status_and_header(DegradedHeaderLayer::new(registry.clone())).await;
        assert_eq!(status, 200);
        assert_eq!(header, None, "healthy platform stamps nothing");

        registry.set(sanvi_resilience::DegradationReason::Redis, true);
        let (status, header) = served_status_and_header(DegradedHeaderLayer::new(registry)).await;
        assert_eq!(status, 200);
        assert_eq!(header.as_deref(), Some("redis"));
    }

    #[tokio::test]
    async fn flag_off_assembly_stamps_nothing() {
        let (status, header) = served_status_and_header(DegradedHeaderLayer::disabled()).await;
        assert_eq!(status, 200);
        assert_eq!(header, None);
    }
}
