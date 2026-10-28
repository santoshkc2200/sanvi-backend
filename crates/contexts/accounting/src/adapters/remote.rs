//! Authenticated HTTP boundary between the platform and Accounting.
//! Claims are bound to the exact request and have a thirty-second lifetime.

use crate::{
    application::{AccountingAccess, Actor, Permission, RequestContext},
    domain::AccountingError,
};
use actix_web::{HttpRequest, HttpResponse, http::StatusCode};
use chrono::Utc;
use sanvi_http::{ApiError, Subject, TenantContext};
use sanvi_telemetry::Redacted;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

pub const DELEGATION_HEADER: &str = "x-accounting-delegation";
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const TENANT_AUDIENCE: &str = "sanvi-accounting";
pub const AUDIT_AUDIENCE: &str = "sanvi-platform-accounting-audit";
/// Tolerated clock drift between hosts. Timestamps are whole seconds, so a
/// sender even slightly ahead would otherwise look like it signed in the future.
pub const MAX_CLOCK_SKEW_SECS: i64 = 5;

/// One side of the platform/Accounting link. Each direction has its own key:
/// `signing_secret` signs requests sent to the peer at `base_url`, and
/// `inbound_secret` verifies requests the peer sends here.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ServiceConfig {
    pub base_url: String,
    pub signing_secret: Redacted<String>,
    pub inbound_secret: Redacted<String>,
}

impl ServiceConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.base_url.is_empty() {
            return Ok(());
        }
        let url = reqwest::Url::parse(&self.base_url)?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "accounting service base_url must be an http(s) origin"
        );
        anyhow::ensure!(
            self.signing_secret.expose_secret().len() >= 32
                && self.inbound_secret.expose_secret().len() >= 32,
            "accounting service signing_secret and inbound_secret must each contain at least 32 bytes"
        );
        anyhow::ensure!(
            self.signing_secret.expose_secret() != self.inbound_secret.expose_secret(),
            "accounting service signing_secret and inbound_secret must differ"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Delegation {
    /// Unique per token. Receivers with side effects reject a repeated ID.
    pub jti: String,
    pub audience: String,
    pub purpose: String,
    pub issued_at: i64,
    pub expires_at: i64,
    pub method: String,
    pub target: String,
    pub fingerprint: String,
    pub tenant: Option<TenantContext>,
    pub subject: Option<Subject>,
    pub permission: Option<String>,
    /// Separately checked at the gateway, including for mutation response reads.
    pub can_read: bool,
    pub request: RequestContext,
}

fn fingerprint(body: &[u8], content_type: &str, if_match: &str) -> String {
    // Length framing prevents ambiguous concatenations of body and headers.
    let mut hash = Sha256::new();
    for part in [body, content_type.as_bytes(), if_match.as_bytes()] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hex::encode(hash.finalize())
}

pub struct RequestBinding<'a> {
    pub method: &'a str,
    pub target: &'a str,
    pub body: &'a [u8],
    pub content_type: &'a str,
    pub if_match: &'a str,
}
impl<'a> RequestBinding<'a> {
    pub fn json(method: &'a str, target: &'a str, body: &'a [u8]) -> Self {
        Self {
            method,
            target,
            body,
            content_type: "application/json",
            if_match: "",
        }
    }
}

impl Delegation {
    pub fn new(audience: &str, purpose: &str, binding: &RequestBinding<'_>) -> Self {
        let RequestBinding {
            method,
            target,
            body,
            content_type,
            if_match,
        } = *binding;
        let now = Utc::now().timestamp();
        Self {
            jti: uuid::Uuid::now_v7().simple().to_string(),
            audience: audience.into(),
            purpose: purpose.into(),
            issued_at: now,
            expires_at: now + 30,
            method: method.into(),
            target: target.into(),
            fingerprint: fingerprint(body, content_type, if_match),
            tenant: None,
            subject: None,
            permission: None,
            can_read: false,
            request: RequestContext::default(),
        }
    }

    pub fn verify(
        secret: &str,
        token: &str,
        audience: &str,
        purpose: &str,
        binding: &RequestBinding<'_>,
    ) -> Result<Self, ApiError> {
        let RequestBinding {
            method,
            target,
            body,
            content_type,
            if_match,
        } = *binding;
        if secret.len() < 32 {
            return Err(ApiError::Unauthorized);
        }
        let claims: Self =
            sanvi_crypto::verify(secret.as_bytes(), token).map_err(|_| ApiError::Unauthorized)?;
        let now = Utc::now().timestamp();
        if claims.jti.is_empty()
            || claims.audience != audience
            || claims.purpose != purpose
            || claims.issued_at > now + MAX_CLOCK_SKEW_SECS
            || claims.expires_at <= now
            || claims.expires_at <= claims.issued_at
            || claims.expires_at - claims.issued_at > 30
            || claims.method != method
            || claims.target != target
            || claims.fingerprint != fingerprint(body, content_type, if_match)
        {
            return Err(ApiError::Unauthorized);
        }
        Ok(claims)
    }

    pub fn from_request(
        secret: &str,
        request: &HttpRequest,
        body: &[u8],
        audience: &str,
        purpose: &str,
    ) -> Result<Self, ApiError> {
        Self::verify(
            secret,
            header(request, DELEGATION_HEADER),
            audience,
            purpose,
            &RequestBinding {
                method: request.method().as_str(),
                target: &request.uri().to_string(),
                body,
                content_type: header(request, "content-type"),
                if_match: header(request, "if-match"),
            },
        )
    }
}

/// Audit delivery body. The command ID is the platform's deduplication key,
/// so two identical entries are never merged and a resent one is never doubled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditCommand {
    pub id: i64,
    pub entry: sanvi_audit::AuditEntryData,
}

pub fn header<'a>(request: &'a HttpRequest, name: &str) -> &'a str {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

pub struct DelegatedAccess(pub Delegation);
#[async_trait::async_trait]
impl AccountingAccess for DelegatedAccess {
    async fn require(&self, actor: Actor, permission: Permission) -> Result<(), AccountingError> {
        let expected = match permission {
            Permission::Read => "tenant.accounting.read",
            Permission::Manage => "tenant.accounting.manage",
            Permission::Export => return Err(AccountingError::Forbidden),
        };
        match (actor, self.0.tenant.as_ref(), self.0.subject.as_ref()) {
            (Actor::User { tenant_id, user_id }, Some(tenant), Some(subject))
                if tenant_id == tenant.tenant_id
                    && subject.tenant_id == Some(tenant_id)
                    && user_id == subject.user_id
                    && (self.0.permission.as_deref() == Some(expected)
                        || (expected == "tenant.accounting.read" && self.0.can_read))
                    && self.0.expires_at > Utc::now().timestamp() =>
            {
                Ok(())
            }
            _ => Err(AccountingError::Forbidden),
        }
    }
}

#[derive(Clone)]
pub struct RemoteClient {
    config: ServiceConfig,
    client: reqwest::Client,
}

pub struct RemoteResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
impl RemoteResponse {
    pub fn into_http(self) -> Result<HttpResponse, ApiError> {
        let status = StatusCode::from_u16(self.status).map_err(|_| ApiError::BadGateway {
            detail: "invalid Accounting response".into(),
        })?;
        let mut response = HttpResponse::build(status);
        for (name, value) in self.headers {
            response.insert_header((name, value));
        }
        Ok(response.body(self.body))
    }
}

impl RemoteClient {
    pub fn new(config: &ServiceConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            config: config.clone(),
            client,
        })
    }
    pub async fn send(
        &self,
        claims: &Delegation,
        body: Vec<u8>,
        content_type: &str,
        if_match: &str,
    ) -> Result<RemoteResponse, ApiError> {
        if self.config.base_url.is_empty() || self.config.signing_secret.expose_secret().len() < 32
        {
            return Err(ApiError::service_unavailable(
                "Accounting service is not configured",
            ));
        }
        let method = reqwest::Method::from_bytes(claims.method.as_bytes()).map_err(|_| {
            ApiError::BadRequest {
                detail: "invalid method".into(),
            }
        })?;
        // The client only accepts absolute paths on the configured origin.
        if !claims.target.starts_with('/') || claims.target.starts_with("//") {
            return Err(ApiError::BadRequest {
                detail: "invalid Accounting target".into(),
            });
        }
        let token = sanvi_crypto::sign(
            self.config.signing_secret.expose_secret().as_bytes(),
            claims,
        );
        let mut builder = self
            .client
            .request(
                method,
                format!(
                    "{}{}",
                    self.config.base_url.trim_end_matches('/'),
                    claims.target
                ),
            )
            .header(DELEGATION_HEADER, token);
        if !content_type.is_empty() {
            builder = builder.header("content-type", content_type);
        }
        if !if_match.is_empty() {
            builder = builder.header("if-match", if_match);
        }
        // No retries: an interrupted mutation can have committed remotely.
        let mut response = builder
            .body(body)
            .send()
            .await
            .map_err(|_| ApiError::service_unavailable("Accounting service is unreachable"))?;
        if response.status().as_u16() == 401 {
            return Err(ApiError::BadGateway {
                detail: "Accounting service rejected platform credentials".into(),
            });
        }
        let headers = response
            .headers()
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.as_str(),
                    "content-type" | "etag" | "retry-after" | "cache-control" | "vary"
                )
            })
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.to_string(), value.to_owned()))
            })
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ApiError::service_unavailable("Accounting response interrupted"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(ApiError::BadGateway {
                    detail: "Accounting response too large".into(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(RemoteResponse {
            status: response.status().as_u16(),
            headers,
            body,
        })
    }
}

#[derive(Clone)]
pub struct GatewayState {
    pub remote: RemoteClient,
    pub access: Arc<dyn AccountingAccess>,
}

pub async fn proxy(
    state: actix_web::web::Data<GatewayState>,
    tenant: sanvi_http::CurrentTenant,
    subject: sanvi_http::CurrentSubject,
    permission: sanvi_http::RequiredPermission,
    _feature: sanvi_http::RequiredAnyFeature,
    request: HttpRequest,
    body: actix_web::web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let required = if request.method().is_safe() {
        Permission::Read
    } else {
        Permission::Manage
    };
    let actor = Actor::User {
        tenant_id: tenant.0.tenant_id,
        user_id: subject.0.user_id,
    };
    state
        .access
        .require(actor, required)
        .await
        .map_err(super::http::api_error)?;
    let can_read = if required == Permission::Read {
        true
    } else {
        match state.access.require(actor, Permission::Read).await {
            Ok(()) => true,
            Err(AccountingError::Forbidden) => false,
            Err(error) => return Err(super::http::api_error(error)),
        }
    };
    let content_type = header(&request, "content-type");
    let if_match = header(&request, "if-match");
    let mut claims = Delegation::new(
        TENANT_AUDIENCE,
        "tenant",
        &RequestBinding {
            method: request.method().as_str(),
            target: &request.uri().to_string(),
            body: &body,
            content_type,
            if_match,
        },
    );
    claims.tenant = Some(tenant.0);
    let mut delegated_subject = subject.0;
    delegated_subject.role_ids.clear();
    claims.subject = Some(delegated_subject);
    claims.can_read = can_read;
    claims.permission = Some(permission.key.to_owned());
    claims.request = super::http::request_context(&request);
    state
        .remote
        .send(&claims, body.to_vec(), content_type, if_match)
        .await?
        .into_http()
}

#[cfg(test)]
mod tests {
    use super::*;
    const SECRET: &str = "accounting-boundary-test-secret-123456789";
    const INBOUND: &str = "accounting-boundary-inbound-secret-987654321";

    #[test]
    fn signed_request_rejects_tampering_and_other_purposes() {
        let binding = RequestBinding {
            method: "PUT",
            target: "/api/v1/tenant/accounting/books/1/switch?x=1",
            body: b"{}",
            content_type: "application/json",
            if_match: "\"2\"",
        };
        let claims = Delegation::new(TENANT_AUDIENCE, "tenant", &binding);
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        assert!(Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "tenant", &binding).is_ok());
        assert!(Delegation::verify(SECRET, &token, AUDIT_AUDIENCE, "audit", &binding).is_err());
        for changed in [
            RequestBinding {
                method: "DELETE",
                ..binding
            },
            RequestBinding {
                target: "/api/v1/tenant/accounting/books/2/switch?x=1",
                ..binding
            },
            RequestBinding {
                target: "/api/v1/tenant/accounting/books/1/switch?x=2",
                ..binding
            },
            RequestBinding {
                body: b"{changed}",
                ..binding
            },
            RequestBinding {
                content_type: "text/plain",
                ..binding
            },
            RequestBinding {
                if_match: "*",
                ..binding
            },
        ] {
            assert!(
                Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "tenant", &changed).is_err()
            );
        }
        assert!(
            Delegation::verify(
                "wrong-key-with-at-least-thirty-two-bytes",
                &token,
                TENANT_AUDIENCE,
                "tenant",
                &binding
            )
            .is_err()
        );
    }

    #[test]
    fn expired_future_and_unbounded_delegations_fail_closed() {
        let binding = RequestBinding::json("POST", "/internal/privacy/review", b"{}");
        let now = Utc::now().timestamp();
        for (issued_at, expires_at) in [(now - 31, now - 1), (now + 10, now + 30), (now, now + 300)]
        {
            let mut claims = Delegation::new(TENANT_AUDIENCE, "privacy", &binding);
            claims.issued_at = issued_at;
            claims.expires_at = expires_at;
            let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
            assert!(
                Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "privacy", &binding).is_err()
            );
        }
        assert!(Delegation::verify("", "", TENANT_AUDIENCE, "privacy", &binding).is_err());
    }

    #[test]
    fn every_delegation_carries_a_distinct_id_and_an_empty_id_fails() {
        let binding = RequestBinding::json("POST", "/internal/privacy/review", b"{}");
        let first = Delegation::new(TENANT_AUDIENCE, "privacy", &binding);
        let second = Delegation::new(TENANT_AUDIENCE, "privacy", &binding);
        assert!(!first.jti.is_empty() && first.jti != second.jti);
        let mut claims = first;
        claims.jti.clear();
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        assert!(Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "privacy", &binding).is_err());
    }

    #[test]
    fn delegations_from_a_slightly_fast_clock_are_accepted() {
        let binding = RequestBinding::json("POST", "/internal/privacy/review", b"{}");
        let now = Utc::now().timestamp();
        let mut claims = Delegation::new(TENANT_AUDIENCE, "privacy", &binding);
        claims.issued_at = now + MAX_CLOCK_SKEW_SECS;
        claims.expires_at = claims.issued_at + 30;
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        assert!(Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "privacy", &binding).is_ok());
    }

    #[test]
    fn endpoint_configuration_rejects_ambiguous_origins_and_weak_keys() {
        for base_url in [
            "file:///tmp/accounting",
            "https://user:password@example.test",
            "https://example.test/prefix",
            "https://example.test?token=x",
            "https://example.test#fragment",
        ] {
            let config = ServiceConfig {
                base_url: base_url.into(),
                signing_secret: Redacted::new(SECRET.into()),
                inbound_secret: Redacted::new(INBOUND.into()),
            };
            assert!(config.validate().is_err(), "{base_url}");
        }
        let mut config = ServiceConfig {
            base_url: "http://accounting:8082".into(),
            signing_secret: Redacted::new("short".into()),
            inbound_secret: Redacted::new(INBOUND.into()),
        };
        assert!(config.validate().is_err());
        config.signing_secret = Redacted::new(INBOUND.into());
        assert!(config.validate().is_err(), "one key for both directions");
        config.signing_secret = Redacted::new(SECRET.into());
        config.inbound_secret = Redacted::new("short".into());
        assert!(config.validate().is_err());
        config.inbound_secret = Redacted::new(INBOUND.into());
        assert!(config.validate().is_ok());
        let debug = format!("{config:?}");
        assert!(!debug.contains(SECRET) && !debug.contains(INBOUND));
    }
}
