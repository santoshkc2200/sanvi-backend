//! Frozen-v1 deprecations share a registry for response headers and release notes.
use std::sync::Arc;

use actix_web::{
    body::BoxBody,
    dev::{ServiceRequest, ServiceResponse},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deprecation {
    pub element: String,
    pub method: String,
    pub path: String,
    pub deprecated_at: DateTime<Utc>,
    pub sunset_at: DateTime<Utc>,
    pub replacement: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DeprecationRegistry {
    pub entries: Vec<Deprecation>,
}

impl DeprecationRegistry {
    pub fn from_json(json: &str) -> Result<Self, String> {
        let registry: Self = serde_json::from_str(json).map_err(|error| error.to_string())?;
        let mut elements = std::collections::HashSet::new();
        for entry in &registry.entries {
            if !elements.insert(&entry.element) || entry.element.trim().is_empty() {
                return Err("deprecation elements must be nonempty and unique".into());
            }
            if !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
                .contains(&entry.method.as_str())
                || !entry.path.starts_with("/api/v1/")
                || entry.replacement.trim().is_empty()
                || entry.replacement.contains(['\r', '\n'])
            {
                return Err(format!(
                    "invalid method, v1 path or replacement for {}",
                    entry.element
                ));
            }
            let earliest = entry
                .deprecated_at
                .checked_add_months(chrono::Months::new(6))
                .ok_or_else(|| format!("deprecation date out of range for {}", entry.element))?;
            if entry.sunset_at < earliest {
                return Err(format!(
                    "{} needs six calendar months before sunset",
                    entry.element
                ));
            }
        }
        Ok(registry)
    }

    pub fn changelog(&self) -> String {
        let mut output = String::from("# API v1 deprecations\n\n");
        if self.entries.is_empty() {
            output.push_str("No API v1 elements are deprecated.\n");
        }
        for entry in &self.entries {
            output.push_str(&format!(
                "- **{}** (`{} {}`): deprecated {}; sunset {}. Replacement: {}.\n",
                entry.element,
                entry.method,
                entry.path,
                entry.deprecated_at.format("%Y-%m-%d"),
                entry.sunset_at.format("%Y-%m-%d"),
                entry.replacement
            ));
        }
        output
    }
}

#[derive(Clone)]
pub struct DeprecationLayer(pub Arc<DeprecationRegistry>);

impl Default for DeprecationLayer {
    fn default() -> Self {
        Self(Arc::new(
            DeprecationRegistry::from_json(include_str!("../../../../api/deprecations.json"))
                .expect("validated deprecation registry"),
        ))
    }
}

impl<S> actix_web::dev::Transform<S, ServiceRequest> for DeprecationLayer
where
    S: actix_web::dev::Service<
            ServiceRequest,
            Response = ServiceResponse<BoxBody>,
            Error = actix_web::Error,
        > + 'static,
{
    type Response = ServiceResponse<BoxBody>;
    type Error = actix_web::Error;
    type Transform = DeprecationService<S>;
    type InitError = ();
    type Future = std::future::Ready<Result<Self::Transform, Self::InitError>>;
    fn new_transform(&self, service: S) -> Self::Future {
        std::future::ready(Ok(DeprecationService {
            service,
            registry: self.0.clone(),
        }))
    }
}

pub struct DeprecationService<S> {
    service: S,
    registry: Arc<DeprecationRegistry>,
}
impl<S> actix_web::dev::Service<ServiceRequest> for DeprecationService<S>
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
        let registry = self.registry.clone();
        let method = req.method().as_str().to_string();
        let future = self.service.call(req);
        Box::pin(async move {
            let mut response = future.await?;
            let path = response
                .request()
                .match_pattern()
                .unwrap_or_else(|| response.request().path().to_string());
            let entry = registry
                .entries
                .iter()
                .filter(|entry| entry.method == method && entry.path == path)
                .min_by_key(|entry| entry.sunset_at);
            if let Some(entry) = entry {
                use actix_web::http::header::{HeaderName, HeaderValue};
                response.headers_mut().insert(
                    HeaderName::from_static("deprecation"),
                    HeaderValue::from_str(&format!("@{}", entry.deprecated_at.timestamp()))
                        .expect("numeric timestamp"),
                );
                response.headers_mut().insert(
                    HeaderName::from_static("sunset"),
                    HeaderValue::from_str(
                        &entry
                            .sunset_at
                            .format("%a, %d %b %Y %H:%M:%S GMT")
                            .to_string(),
                    )
                    .expect("HTTP date"),
                );
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpResponse, test, web};

    const FIXTURE: &str = r#"{"entries":[{"element":"legacyCatalog","method":"GET","path":"/api/v1/catalog/{id}","deprecated_at":"2026-10-09T00:00:00Z","sunset_at":"2027-04-09T00:00:00Z","replacement":"GET /api/v1/catalog/items/{id}"}]}"#;

    #[actix_web::test]
    async fn registry_drives_headers_and_release_changelog_end_to_end() {
        let registry = DeprecationRegistry::from_json(FIXTURE).unwrap();
        let changelog = registry.changelog();
        let app = test::init_service(App::new().wrap(DeprecationLayer(Arc::new(registry))).route(
            "/api/v1/catalog/{id}",
            web::get().to(|| async { HttpResponse::Ok().finish() }),
        ))
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/v1/catalog/42")
                .to_request(),
        )
        .await;
        assert_eq!(
            response
                .headers()
                .get("deprecation")
                .and_then(|v| v.to_str().ok()),
            Some("@1791504000")
        );
        assert_eq!(
            response
                .headers()
                .get("sunset")
                .and_then(|v| v.to_str().ok()),
            Some("Fri, 09 Apr 2027 00:00:00 GMT")
        );
        assert!(changelog.contains("legacyCatalog"));
        assert!(changelog.contains("2027-04-09"));
        assert!(changelog.contains("GET /api/v1/catalog/items/{id}"));
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/v1/catalog/42")
                .to_request(),
        )
        .await;
        assert!(!response.headers().contains_key("deprecation"));
        assert!(!response.headers().contains_key("sunset"));
    }

    #[actix_web::test]
    async fn registry_rejects_a_sunset_shorter_than_six_calendar_months() {
        assert!(
            DeprecationRegistry::from_json(&FIXTURE.replace("2027-04-09", "2027-04-08")).is_err()
        );
    }
}
