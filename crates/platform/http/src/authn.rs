//! Authentication: session verification middleware and the request `Subject`.
//!
//! The platform defines the port ([`SessionVerifier`]) and the shape
//! ([`Subject`]); the identity context supplies the implementation (Kratos
//! whoami + Redis cache). The middleware skips exempt prefixes and, when a
//! verifier is wired, attaches the resolved subject to request extensions
//! and the tracing span. Routes that need an authenticated caller extract
//! [`CurrentSubject`] (or [`OptionalSubject`]); a missing subject is a 401.

use std::{future::Future, pin::Pin, sync::Arc};

use actix_web::{
    HttpMessage,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
    http::header,
};
use sanvi_kernel::{RoleId, TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::ApiError;
use crate::middleware::RequestId;

/// The authenticated subject of a request: who, via which session, at what
/// assurance level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Subject {
    #[schema(value_type = crate::openapi::SnowflakeId)]
    pub user_id: UserId,
    pub session_id: String,
    pub aal: String,
    pub methods: Vec<String>,
    /// Roles held in the *current* tenant (empty on platform routes).
    #[schema(value_type = Option<crate::openapi::SnowflakeId>)]
    pub tenant_id: Option<TenantId>,
    #[schema(value_type = Vec<crate::openapi::SnowflakeId>)]
    pub role_ids: Vec<RoleId>,
    /// When the session was (re)authenticated — the freshness clock for
    /// the fresh-MFA requirement.
    pub authenticated_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The transport channel over which a session token arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionChannel {
    Cookie,
    Bearer,
}

/// Port for session verification, implemented by the identity context.
pub trait SessionVerifier: Send + Sync {
    fn verify<'a>(
        &'a self,
        session_token: Option<(&'a str, SessionChannel)>,
        tenant_id: Option<TenantId>,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Subject>, ApiError>> + Send + 'a>>;
}

/// The session-token extraction: cookie first (Kratos' `ory_kratos_session`),
/// then `Authorization: Bearer` (API tokens).
pub fn session_token_from_request(
    req: &actix_web::HttpRequest,
    cookie_name: &str,
) -> Option<(String, SessionChannel)> {
    if let Some(token) = req
        .cookie(cookie_name)
        .map(|cookie| cookie.value().to_string())
    {
        return Some((token, SessionChannel::Cookie));
    }
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(|token| (token.to_string(), SessionChannel::Bearer))
}

/// Everything the authentication middleware needs.
pub struct AuthnMiddleware {
    pub verifier: Arc<dyn SessionVerifier>,
    /// Cookie name (or None: no cookie channel, bearer only).
    pub session_cookie_name: String,
    /// Path prefixes that never require (nor even attempt) authentication.
    pub exempt_prefixes: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct AuthnLayer {
    parts: Option<Arc<AuthnMiddleware>>,
}

impl AuthnLayer {
    pub fn new(parts: Option<Arc<AuthnMiddleware>>) -> Self {
        Self { parts }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for AuthnLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = AuthnService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(AuthnService {
            service: std::rc::Rc::new(service),
            parts: self.parts.clone(),
        }))
    }
}

pub(crate) struct AuthnService<S> {
    service: std::rc::Rc<S>,
    parts: Option<Arc<AuthnMiddleware>>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for AuthnService<S>
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    actix_web::dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let service = std::rc::Rc::clone(&self.service);
        let Some(parts) = self.parts.clone() else {
            // Identity context not wired: no authentication on any route
            // (the identity feature flag is off).
            return Box::pin(async move { service.call(req).await });
        };
        Box::pin(async move {
            let path = req.path().to_string();
            if parts
                .exempt_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix.as_str()))
            {
                return service.call(req).await;
            }

            let request_id = req.extensions().get::<RequestId>().copied();
            let token = session_token_from_request(req.request(), &parts.session_cookie_name);
            let tenant_id = req
                .extensions()
                .get::<crate::tenancy::TenantContext>()
                .map(|context| context.tenant_id);

            if let Some((token, channel)) = token.as_ref() {
                match parts
                    .verifier
                    .verify(Some((token.as_str(), *channel)), tenant_id)
                    .await
                {
                    Ok(Some(subject)) => {
                        req.extensions_mut().insert(subject.clone());
                        let span = tracing::Span::current();
                        span.record("sanvi.user_id", subject.user_id.to_string());
                        span.record("sanvi.session_id", subject.session_id.clone());
                    }
                    Ok(None) => {
                        // No valid session: the route decides (extractors
                        // reject with 401 where required).
                    }
                    Err(err) => {
                        // Backend trouble (Kratos down, cache poisoned):
                        // fail closed with 503 — never serve an
                        // authenticated route as "anonymous".
                        tracing::error!(
                            error = %err,
                            path = %path,
                            request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                            "session verification backend failure",
                        );
                        let problem = ApiError::ServiceUnavailable {
                            detail: "authentication is temporarily unavailable".to_string(),
                        }
                        .with_context(Some(path), request_id.map(|id| id.to_string()));
                        let response: actix_web::HttpResponse =
                            actix_web::HttpResponse::ServiceUnavailable().json(problem);
                        return Ok(req.into_response(response.map_into_boxed_body()));
                    }
                }
            }

            service.call(req).await
        })
    }
}

/// Extractor: the authenticated subject (401 without one).
#[derive(Clone)]
pub struct CurrentSubject(pub Subject);

impl actix_web::FromRequest for CurrentSubject {
    type Error = ApiError;
    type Future = std::future::Ready<Result<Self, Self::Error>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        match req.extensions().get::<Subject>() {
            Some(subject) => std::future::ready(Ok(Self(subject.clone()))),
            None => std::future::ready(Err(ApiError::Unauthorized)),
        }
    }
}

/// Extractor: the subject when present, `None` otherwise (bootstrap
/// endpoints that render differently for signed-in users).
#[derive(Clone)]
pub struct OptionalSubject(pub Option<Subject>);

impl actix_web::FromRequest for OptionalSubject {
    type Error = ApiError;
    type Future = std::future::Ready<Result<Self, Self::Error>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        std::future::ready(Ok(Self(req.extensions().get::<Subject>().cloned())))
    }
}

/// Extractor: the raw session token (cookie or bearer). Needed by endpoints
/// that must talk to Kratos directly (session list/revocation).
#[derive(Clone)]
pub struct SessionToken(pub String, pub SessionChannel);

impl actix_web::FromRequest for SessionToken {
    type Error = ApiError;
    type Future = std::future::Ready<Result<Self, Self::Error>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        let cookie_name = req
            .app_data::<actix_web::web::Data<AuthnMiddleware>>()
            .map(|parts| parts.session_cookie_name.clone())
            .unwrap_or_else(|| "ory_kratos_session".to_string());
        match session_token_from_request(req, &cookie_name) {
            Some((token, channel)) => std::future::ready(Ok(Self(token, channel))),
            None => std::future::ready(Err(ApiError::Unauthorized)),
        }
    }
}

/// The fresh-MFA policy: how recent a `aal2` authentication must be for
/// dangerous actions (impersonation, role grants, large overrides).
#[derive(Debug, Clone, Copy)]
pub struct FreshAal2Policy {
    pub max_age_secs: i64,
}

impl Default for FreshAal2Policy {
    fn default() -> Self {
        Self { max_age_secs: 300 }
    }
}

/// Extractor for dangerous-action guardrails: the subject must have
/// re-authenticated at AAL2 within the policy window.
#[derive(Clone)]
pub struct FreshAal2Required;

impl actix_web::FromRequest for FreshAal2Required {
    type Error = ApiError;
    type Future = std::future::Ready<Result<Self, Self::Error>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        let extensions = req.extensions();
        let subject = extensions.get::<Subject>();
        let policy = req
            .app_data::<FreshAal2Policy>()
            .copied()
            .unwrap_or_default();

        let fresh = match subject {
            Some(subject) => {
                subject.aal == "aal2"
                    && subject.authenticated_at.is_some_and(|at| {
                        chrono::Utc::now().signed_duration_since(at).num_seconds()
                            <= policy.max_age_secs
                    })
            }
            None => false,
        };
        if fresh {
            std::future::ready(Ok(Self))
        } else {
            std::future::ready(Err(ApiError::Forbidden {
                detail: "fresh aal2 re-authentication required".to_string(),
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionChannel, session_token_from_request};

    /// The channel is what the token arrived on, never a guess from its
    /// value: Kratos accepts a cookie value only as a cookie, and an API
    /// session token only as `X-Session-Token`, and self-hosted Kratos
    /// before v1.0 gave the two no distinguishing prefix.
    #[test]
    fn the_channel_follows_the_transport_not_the_value() {
        let cookie = actix_web::test::TestRequest::default()
            .cookie(actix_web::cookie::Cookie::new(
                "ory_kratos_session",
                "opaque",
            ))
            .to_http_request();
        assert_eq!(
            session_token_from_request(&cookie, "ory_kratos_session"),
            Some(("opaque".to_string(), SessionChannel::Cookie))
        );

        let bearer = actix_web::test::TestRequest::default()
            .insert_header((actix_web::http::header::AUTHORIZATION, "Bearer opaque"))
            .to_http_request();
        assert_eq!(
            session_token_from_request(&bearer, "ory_kratos_session"),
            Some(("opaque".to_string(), SessionChannel::Bearer))
        );

        // The cookie wins when both are present, and its value is not
        // sniffed for a prefix on the way out.
        let both = actix_web::test::TestRequest::default()
            .cookie(actix_web::cookie::Cookie::new(
                "ory_kratos_session",
                "from-cookie",
            ))
            .insert_header((actix_web::http::header::AUTHORIZATION, "Bearer from-header"))
            .to_http_request();
        assert_eq!(
            session_token_from_request(&both, "ory_kratos_session"),
            Some(("from-cookie".to_string(), SessionChannel::Cookie))
        );

        let neither = actix_web::test::TestRequest::default().to_http_request();
        assert_eq!(
            session_token_from_request(&neither, "ory_kratos_session"),
            None
        );
    }
}
