//! Locale resolution: the "what language should this response be in?"
//! middleware.
//!
//! [`LocaleResolver`] is the port: the localization context supplies an
//! implementation (negotiation order, allow-lists, tenant/user settings).
//! The middleware enforces the plumbing around it:
//!
//! - Runs after tenant resolution and impersonation, so the resolver sees
//!   the *effective* subject and tenant.
//! - Never fails a request: negotiation always yields at least the platform
//!   default locale.
//! - On success the [`LocaleContext`] is inserted into request extensions
//!   and recorded on the tracing span; localized responses get
//!   `Content-Language` and `Vary: Accept-Language`.

use std::{future::Future, pin::Pin, sync::Arc};

use actix_web::{
    HttpMessage,
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
    http::header,
};
use sanvi_kernel::TenantId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::authn::Subject;
use crate::tenancy::TenantContext;

/// How the locale was negotiated. Recorded on the context for observability
/// and debugging; the order below matches the negotiation precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocaleSource {
    /// Explicit `?locale=` query parameter (allow-listed).
    Explicit,
    /// The authenticated user's preference.
    UserPreference,
    /// The tenant's configured default.
    TenantDefault,
    /// Best match from the `Accept-Language` header.
    AcceptLanguage,
    /// The platform default.
    PlatformDefault,
}

impl std::fmt::Display for LocaleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Explicit => write!(f, "explicit"),
            Self::UserPreference => write!(f, "user_preference"),
            Self::TenantDefault => write!(f, "tenant_default"),
            Self::AcceptLanguage => write!(f, "accept_language"),
            Self::PlatformDefault => write!(f, "platform_default"),
        }
    }
}

/// The negotiated locale for one request: canonical BCP-47 tag, the source
/// that produced it, the fallback chain (for per-key lookup) and the IANA
/// timezone to render timestamps in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LocaleContext {
    /// Canonical BCP-47 tag, e.g. `en`, `en-GB`, `ja`.
    pub locale: String,
    /// Which negotiation step produced it.
    pub source: LocaleSource,
    /// Fallback chain for per-key lookups, most specific first. The platform
    /// default is always the final entry, so translation can never fail.
    pub fallback_chain: Vec<String>,
    /// IANA timezone (e.g. `Asia/Tokyo`) for rendering timestamps.
    pub timezone: String,
    /// The tenant this locale was negotiated for, when the request was
    /// tenant-scoped.
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
}

impl LocaleContext {
    /// A context for the platform default locale — used by code paths that
    /// run without a request (jobs that never carry a locale, CLI commands).
    pub fn platform_default(locale: impl Into<String>, timezone: impl Into<String>) -> Self {
        let locale = locale.into();
        Self {
            fallback_chain: vec![locale.clone()],
            locale,
            source: LocaleSource::PlatformDefault,
            timezone: timezone.into(),
            tenant_id: None,
        }
    }
}

/// Everything a resolver is allowed to look at. Plain data, no async I/O
/// handles: the implementor owns its own caches and backends.
pub struct LocaleResolutionInput<'a> {
    /// The resolved tenant (absent on public paths).
    pub tenant: Option<&'a TenantContext>,
    /// The effective authenticated subject (absent for anonymous requests).
    pub subject: Option<&'a Subject>,
    /// The `?locale=` query parameter, if present.
    pub explicit_locale: Option<&'a str>,
    /// The raw `Accept-Language` header, if present.
    pub accept_language: Option<&'a str>,
}

/// Port for locale negotiation, implemented by the localization context.
///
/// Infallible: every resolution yields a valid [`LocaleContext`] (the
/// platform default at minimum), because a request must never fail over a
/// missing translation.
pub trait LocaleResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        input: LocaleResolutionInput<'a>,
    ) -> Pin<Box<dyn Future<Output = LocaleContext> + Send + 'a>>;
}

/// Everything the locale middleware needs.
pub struct LocaleMiddleware {
    pub resolver: Arc<dyn LocaleResolver>,
}

pub(crate) struct LocaleResolutionLayer {
    parts: Option<Arc<LocaleMiddleware>>,
}

impl LocaleResolutionLayer {
    pub fn new(parts: Option<Arc<LocaleMiddleware>>) -> Self {
        Self { parts }
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for LocaleResolutionLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = LocaleResolutionService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(LocaleResolutionService {
            service: std::rc::Rc::new(service),
            parts: self.parts.clone(),
        }))
    }
}

pub(crate) struct LocaleResolutionService<S> {
    service: std::rc::Rc<S>,
    parts: Option<Arc<LocaleMiddleware>>,
}

impl<S> actix_web::dev::Service<ServiceRequest> for LocaleResolutionService<S>
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
            // Localization not wired: every response stays in the platform
            // default language (English) with no locale metadata.
            return Box::pin(async move { service.call(req).await });
        };
        Box::pin(async move {
            let query_locale = req
                .query_string()
                .split('&')
                .find_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    (key == "locale").then_some(value)
                })
                .filter(|value| !value.is_empty())
                .map(str::to_string);

            let tenant = req.extensions().get::<TenantContext>().cloned();
            let subject = req.extensions().get::<Subject>().cloned();
            let input = LocaleResolutionInput {
                tenant: tenant.as_ref(),
                subject: subject.as_ref(),
                explicit_locale: query_locale.as_deref(),
                accept_language: req
                    .headers()
                    .get(header::ACCEPT_LANGUAGE)
                    .and_then(|value| value.to_str().ok()),
            };

            let context = parts.resolver.resolve(input).await;

            req.extensions_mut().insert(context.clone());
            let span = tracing::Span::current();
            span.record("sanvi.locale", context.locale.clone());
            span.record("sanvi.locale_source", context.source.to_string());
            span.record("sanvi.locale_timezone", context.timezone.clone());

            let mut res = service.call(req).await?;

            // Localized responses announce their language; caching layers
            // must vary on Accept-Language (and the locale query parameter).
            let headers = res.headers_mut();
            headers.insert(
                header::CONTENT_LANGUAGE,
                header::HeaderValue::from_str(&context.locale)
                    .unwrap_or_else(|_| header::HeaderValue::from_static("en")),
            );
            let vary = headers
                .get(header::VARY)
                .and_then(|value| value.to_str().ok())
                .map(|existing| {
                    if existing
                        .split(',')
                        .any(|entry| entry.trim().eq_ignore_ascii_case("accept-language"))
                    {
                        existing.to_string()
                    } else {
                        format!("{existing}, Accept-Language")
                    }
                })
                .unwrap_or_else(|| "Accept-Language".to_string());
            if let Ok(value) = header::HeaderValue::from_str(&vary) {
                headers.insert(header::VARY, value);
            }

            Ok(res)
        })
    }
}

/// Extractor: the negotiated locale for this request.
///
/// Handlers use this to pick localized output; the middleware guarantees the
/// context is present (or the feature is off and this returns the platform
/// default — never an error).
#[derive(Clone)]
pub struct CurrentLocale(pub LocaleContext);

impl actix_web::FromRequest for CurrentLocale {
    type Error = crate::error::ApiError;
    type Future = std::future::Ready<Result<Self, Self::Error>>;

    fn from_request(req: &actix_web::HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        match req.extensions().get::<LocaleContext>() {
            Some(context) => std::future::ready(Ok(Self(context.clone()))),
            None => std::future::ready(Ok(Self(LocaleContext::platform_default("en", "UTC")))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_source_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&LocaleSource::UserPreference).unwrap(),
            "\"user_preference\""
        );
        assert_eq!(
            serde_json::to_string(&LocaleSource::AcceptLanguage).unwrap(),
            "\"accept_language\""
        );
    }

    #[test]
    fn platform_default_context_is_well_formed() {
        let context = LocaleContext::platform_default("en", "UTC");
        assert_eq!(context.locale, "en");
        assert_eq!(context.source, LocaleSource::PlatformDefault);
        assert_eq!(context.fallback_chain, vec!["en".to_string()]);
    }
}
