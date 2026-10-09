//! Quota counting remains live while rejection is disabled.
use crate::{
    Subject, TenantContext,
    quotas::{QuotaDimension, QuotaService},
};
use actix_web::{
    HttpMessage, ResponseError,
    body::BoxBody,
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    web,
};
use std::{future::Future, pin::Pin, sync::Arc};

#[derive(Clone)]
pub struct QuotaRuntime {
    pub service: Arc<QuotaService>,
    pub enabled: bool,
}
#[derive(Clone)]
pub struct QuotaLayer;
impl<S> Transform<S, ServiceRequest> for QuotaLayer
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = QuotaMiddleware<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;
    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(QuotaMiddleware {
            service: Arc::new(service),
        }))
    }
}
pub struct QuotaMiddleware<S> {
    service: Arc<S>,
}
impl<S> Service<ServiceRequest> for QuotaMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<BoxBody>, Error = actix_web::Error>
        + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;
    actix_web::dev::forward_ready!(service);
    fn call(&self, req: ServiceRequest) -> Self::Future {
        let service = self.service.clone();
        let runtime = req
            .app_data::<web::Data<QuotaRuntime>>()
            .map(|data| data.get_ref().clone());
        let tenant = req.extensions().get::<TenantContext>().and_then(|ctx| {
            req.extensions()
                .get::<Subject>()
                .filter(|subject| {
                    subject.tenant_id == Some(ctx.tenant_id) && !subject.session_id.is_empty()
                })
                .map(|_| ctx.tenant_id)
        });
        Box::pin(async move {
            if let (Some(runtime), Some(tenant)) = (runtime, tenant) {
                let admitted = async {
                    runtime
                        .service
                        .configure_enforcement(tenant, runtime.enabled)
                        .await?;
                    runtime
                        .service
                        .record(tenant, QuotaDimension::ApiRequests, 1, false)
                        .await?;
                    let recovery = req.path() == "/api/v1/tenant/usage"
                        || req.path().starts_with("/api/v1/tenant/billing/");
                    if runtime.enabled && !recovery {
                        runtime
                            .service
                            .enforce(tenant, QuotaDimension::ApiRequests)
                            .await?;
                    }
                    Ok::<_, crate::quotas::QuotaError>(())
                }
                .await;
                if let Err(error) = admitted {
                    if runtime.enabled {
                        return Ok(crate::error::normalize_boundary_error(
                            req.into_response(crate::ApiError::from(error).error_response()),
                        )
                        .await);
                    }
                    tracing::warn!(%error,"quota accounting unavailable in shadow mode");
                }
            }
            service.call(req).await
        })
    }
}
