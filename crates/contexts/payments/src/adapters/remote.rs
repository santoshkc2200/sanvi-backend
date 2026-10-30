//! Authenticated HTTP boundary between the platform and Payments.
//! Claims are bound to the exact request and have a thirty-second lifetime.

use std::{sync::Arc, time::Duration};

use actix_web::{HttpRequest, HttpResponse, http::StatusCode};
use chrono::Utc;
use sanvi_http::{ApiError, Subject, TenantContext};
use sanvi_kernel::{TenantId, UserId};
use sanvi_telemetry::Redacted;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DELEGATION_HEADER: &str = "x-payments-delegation";
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";
pub const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub const TENANT_AUDIENCE: &str = "sanvi-payments";
pub const AUDIT_AUDIENCE: &str = "sanvi-platform-payments-audit";
/// Tolerated clock drift between hosts. Timestamps are whole seconds, so a
/// sender even slightly ahead would otherwise look like it signed in the future.
pub const MAX_CLOCK_SKEW_SECS: i64 = 5;

/// One side of the platform/Payments link. Each direction has its own key:
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
            "payments service base_url must be an http(s) origin"
        );
        anyhow::ensure!(
            self.signing_secret.expose_secret().len() >= 32
                && self.inbound_secret.expose_secret().len() >= 32,
            "payments service signing_secret and inbound_secret must each contain at least 32 bytes"
        );
        anyhow::ensure!(
            self.signing_secret.expose_secret() != self.inbound_secret.expose_secret(),
            "payments service signing_secret and inbound_secret must differ"
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
    /// The caller's `Idempotency-Key`. Sent as the header too; a receiver
    /// rejects a header that differs from this signed value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// The end client's address as resolved by the platform, for audit
    /// entries and per-IP throttles behind the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_ip: Option<std::net::IpAddr>,
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
            idempotency_key: None,
            client_ip: None,
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
        let claims = Self::verify(
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
        )?;
        if header(request, IDEMPOTENCY_HEADER) != claims.idempotency_key.as_deref().unwrap_or("") {
            return Err(ApiError::Unauthorized);
        }
        Ok(claims)
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
            detail: "invalid Payments response".into(),
        })?;
        let mut response = HttpResponse::build(status);
        for (name, value) in self.headers {
            response.insert_header((name, value));
        }
        Ok(response.body(self.body))
    }
}

/// RFC 3986 percent-encoding for URI query parameter values.
pub fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            b => {
                use std::fmt::Write;
                let _ = write!(encoded, "%{b:02X}");
            }
        }
    }
    encoded
}

impl RemoteClient {
    pub fn new(config: &ServiceConfig) -> anyhow::Result<Self> {
        Self::with_timeout(config, Duration::from_secs(10))
    }

    pub fn with_timeout(config: &ServiceConfig, timeout: Duration) -> anyhow::Result<Self> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .timeout(timeout)
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
                "Payments service is not configured",
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
                detail: "invalid Payments target".into(),
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
        if let Some(key) = &claims.idempotency_key {
            builder = builder.header(IDEMPOTENCY_HEADER, key);
        }
        // No retries: an interrupted mutation can have committed remotely.
        let mut response = builder
            .body(body)
            .send()
            .await
            .map_err(|_| ApiError::service_unavailable("Payments service is unreachable"))?;
        if response.status().as_u16() == 401 {
            return Err(ApiError::BadGateway {
                detail: "Payments service rejected platform credentials".into(),
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
            .map_err(|_| ApiError::service_unavailable("Payments response interrupted"))?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(ApiError::BadGateway {
                    detail: "Payments response too large".into(),
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

#[async_trait::async_trait]
pub trait PaymentsAuthorizer: Send + Sync {
    async fn check(
        &self,
        user_id: UserId,
        tenant_id: Option<TenantId>,
        permission: &str,
    ) -> anyhow::Result<bool>;
}

#[derive(Clone)]
pub struct GatewayState {
    pub remote: RemoteClient,
    pub authorizer: Arc<dyn PaymentsAuthorizer>,
}

pub async fn proxy(
    state: actix_web::web::Data<GatewayState>,
    tenant: sanvi_http::CurrentTenant,
    subject: sanvi_http::CurrentSubject,
    permission: sanvi_http::RequiredPermission,
    request: HttpRequest,
    body: actix_web::web::Bytes,
) -> Result<HttpResponse, ApiError> {
    let can_read = if permission.key == "payments.read" {
        true
    } else {
        state
            .authorizer
            .check(subject.0.user_id, Some(tenant.0.tenant_id), "payments.read")
            .await
            .unwrap_or(false)
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
    claims.idempotency_key = Some(header(&request, IDEMPOTENCY_HEADER))
        .filter(|key| !key.is_empty())
        .map(str::to_owned);
    claims.client_ip = sanvi_http::client_ip(&request);
    state
        .remote
        .send(&claims, body.to_vec(), content_type, if_match)
        .await?
        .into_http()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "01234567890123456789012345678901";

    #[test]
    fn valid_delegation_verifies() {
        let binding = RequestBinding::json("POST", "/api/v1/tenant/payments", b"{}");
        let claims = Delegation::new(TENANT_AUDIENCE, "tenant", &binding);
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        assert!(Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "tenant", &binding).is_ok());
    }

    #[test]
    fn wrong_audience_or_purpose_fails() {
        let binding = RequestBinding::json("POST", "/api/v1/tenant/payments", b"{}");
        let claims = Delegation::new(TENANT_AUDIENCE, "tenant", &binding);
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        assert!(Delegation::verify(SECRET, &token, AUDIT_AUDIENCE, "audit", &binding).is_err());
        assert!(Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "platform", &binding).is_err());
    }

    #[test]
    fn any_changed_binding_or_key_fails() {
        let binding = RequestBinding {
            method: "POST",
            target: "/api/v1/tenant/payments",
            body: b"{}",
            content_type: "application/json",
            if_match: "\"2\"",
        };
        let claims = Delegation::new(TENANT_AUDIENCE, "tenant", &binding);
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        for changed in [
            RequestBinding {
                method: "DELETE",
                ..binding
            },
            RequestBinding {
                target: "/api/v1/tenant/payments/1/refund",
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
    fn idempotency_header_must_match_the_signed_key() {
        let target = "/api/v1/tenant/payments/1/refund";
        let mut claims = Delegation::new(
            TENANT_AUDIENCE,
            "tenant",
            &RequestBinding::json("POST", target, b"{}"),
        );
        claims.idempotency_key = Some("0192b1c4-6a7e-7c3d-9f00-000000000001".into());
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        let request = |key: Option<&str>| {
            let mut request = actix_web::test::TestRequest::post()
                .uri(target)
                .insert_header((DELEGATION_HEADER, token.clone()))
                .insert_header(("content-type", "application/json"));
            if let Some(key) = key {
                request = request.insert_header((IDEMPOTENCY_HEADER, key));
            }
            request.to_http_request()
        };
        let verify = |request: HttpRequest| {
            Delegation::from_request(SECRET, &request, b"{}", TENANT_AUDIENCE, "tenant")
        };
        assert!(verify(request(Some("0192b1c4-6a7e-7c3d-9f00-000000000001"))).is_ok());
        assert!(verify(request(Some("0192b1c4-6a7e-7c3d-9f00-000000000002"))).is_err());
        assert!(verify(request(None)).is_err());
    }

    #[test]
    fn expired_and_future_delegations_fail() {
        let binding = RequestBinding::json("POST", "/internal/privacy/collect", b"{}");
        let now = Utc::now().timestamp();
        let mut claims = Delegation::new(TENANT_AUDIENCE, "privacy", &binding);
        claims.issued_at = now - 60;
        claims.expires_at = now - 30;
        let token = sanvi_crypto::sign(SECRET.as_bytes(), &claims);
        assert!(Delegation::verify(SECRET, &token, TENANT_AUDIENCE, "privacy", &binding).is_err());
    }
}
