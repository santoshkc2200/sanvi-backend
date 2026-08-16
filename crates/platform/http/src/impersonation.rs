//! Impersonation: the support operator's scoped, time-boxed, read-only-by-
//! default session as a tenant user.
//!
//! The flow: a platform admin creates a grant (POST
//! `/api/v1/platform/impersonations`, fresh MFA); requests made with
//! `x-sanvi-impersonate: {grant_id}` are then resolved by this middleware
//! into the *target* subject — the permission gate and handlers see the
//! tenant user, while [`ImpersonationContext`] records who is really
//! acting. Every impersonated response carries the banner header
//! `x-sanvi-impersonation` so the UI can flag the session; read-only grants
//! reject unsafe methods with 403.

use std::{future::Future, pin::Pin, sync::Arc};

use actix_web::{
    HttpMessage, ResponseError,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::authn::Subject;
use crate::error::ApiError;

/// Request header carrying the grant id.
pub const IMPERSONATION_HEADER: &str = "x-sanvi-impersonate";
/// Response header the UI uses to banner the impersonated session.
pub const IMPERSONATION_BANNER_HEADER: &str = "x-sanvi-impersonation";

/// How much the impersonated session may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImpersonationMode {
    /// Unsafe methods (anything but GET/HEAD/OPTIONS) are rejected.
    ReadOnly,
    /// Full tenant-user privileges within the grant's tenant.
    ReadWrite,
}

impl ImpersonationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ReadWrite => "read_write",
        }
    }
}

/// A valid, active impersonation grant resolved for the current request.
#[derive(Debug, Clone)]
pub struct ImpersonatedSubject {
    pub grant_id: Uuid,
    pub actor_user_id: Uuid,
    pub target_user_id: Uuid,
    pub tenant_id: Uuid,
    pub role_ids: Vec<Uuid>,
    pub mode: ImpersonationMode,
}

/// Port for grant validation, implemented by the `platform_admin` context:
/// the grant must exist, be unrevoked and unexpired, and the target user
/// must still be an active member of the tenant.
pub trait ImpersonationVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        grant_id: Uuid,
        actor: &'a Subject,
        tenant_id: Option<Uuid>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ImpersonatedSubject>, ApiError>> + Send + 'a>>;
}

/// The impersonation context a request ran under (inserted by the
/// middleware; read by handlers that must record `on_behalf_of`).
#[derive(Debug, Clone)]
pub struct ImpersonationContext {
    pub grant_id: Uuid,
    pub actor_user_id: Uuid,
    pub target_user_id: Uuid,
    pub mode: ImpersonationMode,
}

/// Convenience: the impersonating actor for audit attribution, when the
/// request ran under impersonation.
pub fn impersonation_actor(req: &actix_web::HttpRequest) -> Option<Uuid> {
    req.extensions()
        .get::<ImpersonationContext>()
        .map(|context| context.actor_user_id)
}

/// Everything the impersonation middleware needs.
pub struct ImpersonationMiddleware {
    pub verifier: Arc<dyn ImpersonationVerifier>,
}

#[derive(Clone)]
pub(crate) struct ImpersonationLayer {
    parts: Option<Arc<ImpersonationMiddleware>>,
}

impl ImpersonationLayer {
    pub fn new(parts: Option<Arc<ImpersonationMiddleware>>) -> Self {
        Self { parts }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for ImpersonationLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = ImpersonationService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(ImpersonationService {
            service: std::rc::Rc::new(service),
            parts: self.parts.clone(),
        }))
    }
}

pub(crate) struct ImpersonationService<S> {
    service: std::rc::Rc<S>,
    parts: Option<Arc<ImpersonationMiddleware>>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for ImpersonationService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let service = std::rc::Rc::clone(&self.service);
        let Some(parts) = self.parts.clone() else {
            // platform_admin context not wired: no impersonation possible.
            return Box::pin(async move { service.call(req).await });
        };
        Box::pin(async move {
            let path = req.path().to_string();
            let Some(raw) = req
                .headers()
                .get(IMPERSONATION_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
            else {
                return service.call(req).await;
            };

            let Ok(grant_id) = raw.parse::<Uuid>() else {
                return Ok(req.into_response(
                    ApiError::BadRequest {
                        detail: "invalid impersonation grant id".to_string(),
                    }
                    .error_response()
                    .map_into_boxed_body(),
                ));
            };

            let Some(subject) = req.extensions().get::<Subject>().cloned() else {
                return Ok(req.into_response(
                    ApiError::Unauthorized
                        .error_response()
                        .map_into_boxed_body(),
                ));
            };
            let tenant_id = req
                .extensions()
                .get::<crate::tenancy::TenantContext>()
                .map(|context| context.tenant_id);

            match parts.verifier.verify(grant_id, &subject, tenant_id).await {
                Ok(Some(impersonated)) => {
                    if impersonated.mode == ImpersonationMode::ReadOnly && !req.method().is_safe() {
                        tracing::warn!(
                            grant_id = %grant_id,
                            path = %path,
                            "impersonation rejected: read-only grant attempted a write"
                        );
                        return Ok(req.into_response(
                            ApiError::Forbidden {
                                detail: "impersonation grant is read-only".to_string(),
                            }
                            .error_response()
                            .map_into_boxed_body(),
                        ));
                    }
                    req.extensions_mut().insert(ImpersonationContext {
                        grant_id,
                        actor_user_id: impersonated.actor_user_id,
                        target_user_id: impersonated.target_user_id,
                        mode: impersonated.mode,
                    });
                    // The effective subject is the target user, with their
                    // tenant roles; the session/aal stay the operator's
                    // (MFA guarantees came from the grant creation).
                    let effective = Subject {
                        user_id: impersonated.target_user_id,
                        session_id: subject.session_id.clone(),
                        aal: subject.aal.clone(),
                        methods: subject.methods.clone(),
                        tenant_id: Some(impersonated.tenant_id),
                        role_ids: impersonated.role_ids.clone(),
                        authenticated_at: subject.authenticated_at,
                    };
                    req.extensions_mut().insert(effective.clone());
                    let span = tracing::Span::current();
                    span.record("sanvi.user_id", effective.user_id.to_string());
                    span.record(
                        "sanvi.impersonated_by",
                        impersonated.actor_user_id.to_string(),
                    );

                    let mut res = service.call(req).await?;
                    res.headers_mut().insert(
                        actix_web::http::header::HeaderName::from_static(
                            IMPERSONATION_BANNER_HEADER,
                        ),
                        actix_web::http::header::HeaderValue::from_str(&format!(
                            "{}:{}",
                            grant_id,
                            impersonated.mode.as_str()
                        ))
                        .unwrap_or_else(|_| {
                            actix_web::http::header::HeaderValue::from_static("unknown")
                        }),
                    );
                    Ok(res)
                }
                Ok(None) => Ok(req.into_response(
                    ApiError::Forbidden {
                        detail: "impersonation grant is invalid, expired or revoked".to_string(),
                    }
                    .error_response()
                    .map_into_boxed_body(),
                )),
                Err(err) => {
                    tracing::error!(
                        error = %err,
                        grant_id = %grant_id,
                        "impersonation verification backend failure"
                    );
                    Ok(req.into_response(err.error_response().map_into_boxed_body()))
                }
            }
        })
    }
}
