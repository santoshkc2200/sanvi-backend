//! Kratos webhook receivers: `/internal/kratos/*`.
//!
//! Not part of the public API. Protected twice over:
//! 1. A shared secret in the `x-sanvi-webhook` header, compared in constant
//!    time; without a configured secret the receivers refuse everything.
//! 2. A client-IP allow-list (loopback + the compose network by default).
//!
//! The registration/login/settings hooks are `can_interrupt: true` on the
//! Kratos side: a non-2xx response here blocks the flow, which is how we
//! guarantee no Kratos identity exists without a local projection.

use actix_web::{HttpMessage, HttpRequest, HttpResponse, Responder, web};
use sanvi_http::{ApiError, RequestId};

use crate::adapters::kratos::kratos_client_impl::WebhookIdentity;
use crate::application::{Flow, SyncIdentityFromKratos, SyncIdentityInput};
use crate::domain::IdentityError;
use crate::domain::values::{Email, Locale};

/// Everything the webhook receivers need.
#[derive(Clone)]
pub struct WebhookState {
    pub sync: std::sync::Arc<SyncIdentityFromKratos>,
    pub logout: std::sync::Arc<crate::application::HandleLogoutWebhook>,
    pub secret: String,
    /// Allowed client IPs (CIDR or exact) — loopback and the compose subnet
    /// in local development.
    pub allowed_clients: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct WebhookPayload {
    #[serde(default)]
    pub flow: Option<String>,
    #[serde(default)]
    pub identity: Option<WebhookIdentity>,
    #[serde(default)]
    pub session: Option<WebhookSession>,
}

#[derive(Debug, serde::Deserialize)]
pub struct WebhookSession {
    #[serde(default)]
    pub id: Option<String>,
}

/// Verify the shared secret in constant time (no early exit on mismatch).
fn secret_matches(expected: &str, actual: Option<&str>) -> bool {
    if expected.is_empty() {
        return false;
    }
    let actual = actual.unwrap_or_default().as_bytes();
    let expected = expected.as_bytes();
    let mut diff = expected.len() ^ actual.len();
    for (a, b) in expected.iter().zip(actual.iter()) {
        diff |= (*a ^ *b) as usize;
    }
    diff == 0
}

fn client_allowed(allowed: &[String], client_ip: &str) -> bool {
    if allowed.is_empty() {
        // Empty allow-list = fail closed, never open.
        return false;
    }
    allowed.iter().any(|entry| {
        if let Some((network, bits)) = entry.split_once('/') {
            match (network.parse::<std::net::Ipv4Addr>(), bits.parse::<u8>()) {
                (Ok(network), Ok(bits)) if bits <= 32 => {
                    let mask = u32::MAX << (32 - bits);
                    let network_u32 = u32::from(network) & mask;
                    client_ip
                        .parse::<std::net::Ipv4Addr>()
                        .map(|ip| u32::from(ip) & mask == network_u32)
                        .unwrap_or(false)
                }
                _ => false,
            }
        } else {
            entry == client_ip
        }
    })
}

fn client_ip(req: &HttpRequest) -> String {
    req.connection_info()
        .realip_remote_addr()
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string())
}

fn check_webhook_auth(
    state: &WebhookState,
    req: &HttpRequest,
) -> Result<(), actix_web::HttpResponse> {
    let secret = req
        .headers()
        .get("x-sanvi-webhook")
        .and_then(|value| value.to_str().ok());
    if !secret_matches(&state.secret, secret) {
        tracing::warn!(client_ip = %client_ip(req), "webhook rejected: bad or missing shared secret");
        return Err(deny(ApiError::Forbidden {
            detail: "invalid webhook secret".to_string(),
        }));
    }
    let ip = client_ip(req);
    if !client_allowed(&state.allowed_clients, &ip) {
        tracing::warn!(client_ip = %ip, "webhook rejected: client ip not allowed");
        return Err(deny(ApiError::Forbidden {
            detail: "client not allowed".to_string(),
        }));
    }
    Ok(())
}

fn deny(error: ApiError) -> actix_web::HttpResponse {
    HttpResponse::build(error.status()).json(error.problem())
}

/// Parse the shared shape into a sync input (used by the three interrupting
/// hooks).
fn to_sync_input(payload: &WebhookPayload, flow: Flow) -> Result<SyncIdentityInput, IdentityError> {
    let identity = payload
        .identity
        .as_ref()
        .ok_or_else(|| IdentityError::MalformedWebhook("no identity in webhook payload".into()))?;
    let kratos_identity_id = identity
        .id
        .parse::<uuid::Uuid>()
        .map_err(|_| IdentityError::MalformedWebhook("identity id is not a uuid".into()))?;
    let email = identity
        .email()
        .map(Email::parse)
        .transpose()?
        .ok_or_else(|| IdentityError::MalformedWebhook("identity has no email trait".into()))?;
    let locale = identity
        .traits
        .locale
        .as_deref()
        // Empty strings are a common mapper artifact; treat them as absent.
        .filter(|locale| !locale.is_empty())
        .map(Locale::parse)
        .transpose()
        .map_err(|_| {
            IdentityError::InvalidLocale(identity.traits.locale.clone().unwrap_or_default())
        })?;
    Ok(SyncIdentityInput {
        flow,
        kratos_identity_id,
        email,
        email_verified: identity.email_verified(),
        locale,
        oidc_identifiers: identity
            .oidc_identifiers()
            .into_iter()
            .map(|(provider, subject)| format!("{}:{}", provider.as_str(), subject.as_str()))
            .collect(),
        session_id: payload
            .session
            .as_ref()
            .and_then(|session| session.id.clone()),
    })
}

fn error_response(err: IdentityError, req: &HttpRequest) -> actix_web::HttpResponse {
    let request_id = req.extensions().get::<RequestId>().map(|id| id.to_string());
    let instance = req.path().to_string();
    let error = super::handlers::api_error(err);
    HttpResponse::build(error.status()).json(error.with_context(Some(instance), request_id))
}

/// `POST /internal/kratos/registration` — projection upsert (interrupting).
///
/// Registration webhooks fire pre-persist too (identity.id still zero);
/// those calls are acknowledged without side effects — the post-persist
/// call carries the real id.
pub async fn kratos_registration(
    state: web::Data<WebhookState>,
    req: HttpRequest,
    body: web::Json<WebhookPayload>,
) -> impl Responder {
    if let Err(response) = check_webhook_auth(&state, &req) {
        return response;
    }
    match to_sync_input(&body, Flow::Registration) {
        Ok(input) if input.kratos_identity_id.is_nil() => {
            tracing::debug!("registration webhook fired pre-persist; skipping");
            HttpResponse::NoContent().finish()
        }
        Ok(input) => match state.sync.execute(input).await {
            Ok(()) => HttpResponse::NoContent().finish(),
            Err(err) => error_response(err, &req),
        },
        Err(err) => error_response(err, &req),
    }
}

/// `POST /internal/kratos/login` — refresh + auto-link + session event
/// (interrupting).
pub async fn kratos_login(
    state: web::Data<WebhookState>,
    req: HttpRequest,
    body: web::Json<WebhookPayload>,
) -> impl Responder {
    if let Err(response) = check_webhook_auth(&state, &req) {
        return response;
    }
    match to_sync_input(&body, Flow::Login) {
        Ok(input) => match state.sync.execute(input).await {
            Ok(()) => HttpResponse::NoContent().finish(),
            Err(err) => error_response(err, &req),
        },
        Err(err) => error_response(err, &req),
    }
}

/// `POST /internal/kratos/settings` — refresh projection (interrupting).
pub async fn kratos_settings(
    state: web::Data<WebhookState>,
    req: HttpRequest,
    body: web::Json<WebhookPayload>,
) -> impl Responder {
    if let Err(response) = check_webhook_auth(&state, &req) {
        return response;
    }
    match to_sync_input(&body, Flow::Settings) {
        Ok(input) => match state.sync.execute(input).await {
            Ok(()) => HttpResponse::NoContent().finish(),
            Err(err) => error_response(err, &req),
        },
        Err(err) => error_response(err, &req),
    }
}

/// `POST /internal/kratos/logout` — session cache invalidation (best effort;
/// Kratos never blocks logout on this hook's failure).
pub async fn kratos_logout(
    state: web::Data<WebhookState>,
    req: HttpRequest,
    body: web::Json<WebhookPayload>,
) -> impl Responder {
    if let Err(response) = check_webhook_auth(&state, &req) {
        return response;
    }
    let session_id = body.session.as_ref().and_then(|session| session.id.clone());
    let kratos_identity_id = body
        .identity
        .as_ref()
        .and_then(|identity| identity.id.parse::<uuid::Uuid>().ok());
    match state.logout.execute(session_id, kratos_identity_id).await {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(err) => error_response(err, &req),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_secret_check() {
        assert!(secret_matches("s3cret", Some("s3cret")));
        assert!(!secret_matches("s3cret", Some("wrong")));
        assert!(!secret_matches("s3cret", None));
        assert!(!secret_matches("", Some("")));
        assert!(!secret_matches("s3cret", Some("s3cret-longer")));
        assert!(!secret_matches("s3cret-longer", Some("s3cret")));
    }

    #[test]
    fn client_allowlist_semantics() {
        let allowed = vec!["127.0.0.1".to_string(), "172.16.0.0/12".to_string()];
        assert!(client_allowed(&allowed, "127.0.0.1"));
        assert!(client_allowed(&allowed, "172.31.255.255"));
        assert!(!client_allowed(&allowed, "8.8.8.8"));
        assert!(!client_allowed(&[], "127.0.0.1"));
    }

    #[test]
    fn payload_without_identity_is_malformed() {
        let payload = WebhookPayload {
            flow: Some("login".into()),
            identity: None,
            session: None,
        };
        assert!(matches!(
            to_sync_input(&payload, Flow::Login),
            Err(IdentityError::MalformedWebhook(_))
        ));
    }
}
