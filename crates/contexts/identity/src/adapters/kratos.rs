//! The Kratos REST adapter: session verification, revocation and identity
//! metadata. Pins no SDK — the surface we use is small and stable (public
//! `whoami` + session revocation, admin identity PATCH).

use async_trait::async_trait;
use reqwest::header;
use serde::Deserialize;
use tracing::warn;

use crate::domain::ports::{KratosClient, SessionInfo};
use crate::domain::values::{Email, Locale, ProviderId, ProviderSubject};
use crate::domain::{IdentityError, UserId};

#[derive(Debug, Clone)]
pub struct HttpKratosClient {
    http: reqwest::Client,
    public_url: String,
    admin_url: Option<String>,
    session_cookie_name: String,
}

const KRATOS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl HttpKratosClient {
    pub fn new(
        public_url: String,
        admin_url: Option<String>,
        session_cookie_name: String,
    ) -> Result<Self, IdentityError> {
        Self::new_with_timeout(public_url, admin_url, session_cookie_name, KRATOS_TIMEOUT)
    }

    /// Test constructor: a shortened deadline so a slow Kratos is
    /// observable in milliseconds. The transport contract (deadline,
    /// fail-closed) is identical.
    pub fn new_with_timeout(
        public_url: String,
        admin_url: Option<String>,
        session_cookie_name: String,
        timeout: std::time::Duration,
    ) -> Result<Self, IdentityError> {
        let http = reqwest::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|err| IdentityError::KratosUnreachable(err.to_string()))?;
        Ok(Self {
            http,
            public_url,
            admin_url,
            session_cookie_name,
        })
    }

    /// The `whoami` request, built but not sent.
    ///
    /// The token arrives as either an API session token (sent via
    /// `X-Session-Token`) or the raw browser cookie value, which Kratos only
    /// accepts as a literal session cookie under its *configured* name.
    /// They are not interchangeable: a cookie value sent as
    /// `X-Session-Token` makes Kratos reject the whole request even when the
    /// cookie header is valid, so exactly one channel goes out per call.
    fn whoami_request(
        &self,
        session_token: &str,
        channel: sanvi_http::SessionChannel,
    ) -> reqwest::RequestBuilder {
        let request = self
            .http
            .get(format!("{}/sessions/whoami", self.public_url));
        match channel {
            sanvi_http::SessionChannel::Bearer => request.header("X-Session-Token", session_token),
            sanvi_http::SessionChannel::Cookie => request.header(
                header::COOKIE,
                format!("{}={session_token}", self.session_cookie_name),
            ),
        }
    }

    /// Parse an OIDC credentials identifier list into provider/subject pairs.
    pub fn parse_oidc_identifiers(identifiers: &[String]) -> Vec<(ProviderId, ProviderSubject)> {
        identifiers
            .iter()
            .filter_map(|identifier| {
                let (provider, subject) = identifier.split_once(':')?;
                let provider = ProviderId::parse(provider).ok()?;
                if subject.is_empty() {
                    return None;
                }
                Some((provider, ProviderSubject::new(subject.to_string())))
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct WhoamiResponse {
    id: String,
    #[serde(default)]
    authenticated_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Kratos's own assurance level for the session — the source of truth
    /// for aal (the per-method list starts with the FIRST factor, so a
    /// naive first-method read would always report aal1).
    #[serde(default)]
    authenticator_assurance_level: Option<String>,
    identity: KratosIdentity,
    #[serde(default)]
    authentication_methods: Vec<AuthenticationMethod>,
}

#[derive(Debug, Deserialize)]
struct KratosIdentity {
    id: String,
    traits: Traits,
    #[serde(default)]
    verifiable_addresses: Vec<VerifiableAddress>,
    #[serde(default)]
    credentials: Option<Credentials>,
}

#[derive(Debug, Deserialize)]
struct Traits {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    locale: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VerifiableAddress {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    verified: bool,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
pub struct Credentials {
    #[serde(default)]
    oidc: Option<OidcCredentials>,
}

// TASK-024 11.5f: Kratos credential material (provider identifiers, hashed
// credential config in real payloads) must never render. Manual Debug so any
// containing struct's derived Debug shows the redaction marker instead.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials([REDACTED])")
    }
}

#[derive(Debug, Deserialize)]
struct OidcCredentials {
    #[serde(default)]
    identifiers: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthenticationMethod {
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    aal: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

impl WhoamiResponse {
    fn into_session_info(self) -> Result<SessionInfo, IdentityError> {
        let email = self
            .identity
            .traits
            .email
            .as_deref()
            .map(Email::parse)
            .transpose()?
            .ok_or_else(|| IdentityError::MalformedWebhook("identity has no email trait".into()))?;

        let email_verified = self.identity.verifiable_addresses.iter().any(|address| {
            address.verified
                && address.status.as_deref() == Some("completed")
                && address.value.as_deref() == Some(email.as_str())
        });

        let locale = self
            .identity
            .traits
            .locale
            .as_deref()
            .map(Locale::parse)
            .transpose()
            .ok()
            .flatten();

        let aal = self
            .authenticator_assurance_level
            .clone()
            .filter(|level| !level.is_empty())
            .unwrap_or_else(|| {
                // Fall back to the strongest aal across the method list.
                self.authentication_methods
                    .iter()
                    .filter_map(|method| method.aal.clone())
                    .max()
                    .unwrap_or_else(|| "aal1".to_string())
            });

        let methods: Vec<String> = self
            .authentication_methods
            .iter()
            .filter_map(|method| {
                let method_name = method.method.clone()?;
                match method_name.as_str() {
                    "oidc" => Some(format!(
                        "oidc:{}",
                        method
                            .provider
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string())
                    )),
                    other => Some(other.to_string()),
                }
            })
            .collect();

        let identity_id = self
            .identity
            .id
            .parse::<uuid::Uuid>()
            .map_err(|_| IdentityError::MalformedWebhook("identity id is not a uuid".into()))?;

        let oidc_identifiers = self
            .identity
            .credentials
            .and_then(|credentials| credentials.oidc)
            .map(|oidc| HttpKratosClient::parse_oidc_identifiers(&oidc.identifiers))
            .unwrap_or_default();

        Ok(SessionInfo {
            session_id: self.id,
            identity_id,
            email,
            email_verified,
            locale,
            aal,
            methods,
            authenticated_at: self.authenticated_at,
            oidc_identifiers,
        })
    }
}

/// Shared parse helper (also used by the webhook receiver).
pub mod kratos_client_impl {
    use super::*;

    pub fn parse_oidc_identifiers(identifiers: &[String]) -> Vec<(ProviderId, ProviderSubject)> {
        HttpKratosClient::parse_oidc_identifiers(identifiers)
    }

    /// Parse a Kratos webhook payload's identity snapshot into the same
    /// normalized shape the session path uses. `None` for fields Kratos
    /// omitted; callers decide what is required.
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub struct WebhookIdentity {
        pub id: String,
        #[serde(default)]
        pub traits: WebhookTraits,
        #[serde(default)]
        pub verifiable_addresses: Vec<VerifiableAddress>,
        #[serde(default)]
        pub credentials: Option<Credentials>,
    }

    #[derive(Debug, Default, Deserialize)]
    pub struct WebhookTraits {
        #[serde(default)]
        pub email: Option<String>,
        #[serde(default)]
        pub locale: Option<String>,
    }

    impl WebhookIdentity {
        pub fn email(&self) -> Option<&str> {
            self.traits.email.as_deref()
        }

        pub fn email_verified(&self) -> bool {
            let Some(email) = self.email() else {
                return false;
            };
            self.verifiable_addresses.iter().any(|address| {
                address.verified
                    && address.status.as_deref() == Some("completed")
                    && address.value.as_deref() == Some(email)
            })
        }

        pub fn oidc_identifiers(&self) -> Vec<(ProviderId, ProviderSubject)> {
            self.credentials
                .as_ref()
                .and_then(|credentials| credentials.oidc.as_ref())
                .map(|oidc| parse_oidc_identifiers(&oidc.identifiers))
                .unwrap_or_default()
        }
    }
}

#[async_trait]
impl KratosClient for HttpKratosClient {
    async fn whoami(
        &self,
        session_token: &str,
        channel: sanvi_http::SessionChannel,
    ) -> Result<Option<SessionInfo>, IdentityError> {
        let response = self
            .whoami_request(session_token, channel)
            .send()
            .await
            .map_err(|err| IdentityError::KratosUnreachable(err.to_string()))?;

        match response.status() {
            reqwest::StatusCode::OK => {
                let body: WhoamiResponse = response
                    .json()
                    .await
                    .map_err(|err| IdentityError::MalformedWebhook(err.to_string()))?;
                Ok(Some(body.into_session_info()?))
            }
            reqwest::StatusCode::UNAUTHORIZED => Ok(None),
            other => Err(IdentityError::KratosRejected(format!(
                "whoami returned {other}"
            ))),
        }
    }

    async fn revoke_session(&self, session_id: &str) -> Result<(), IdentityError> {
        // Kratos's public API refuses to revoke the session a request is
        // authenticated with ("use the logout flow instead"), so
        // self-revocation goes through the admin endpoint. The caller
        // guarantees the session belongs to the authenticated user via a
        // whoami check first.
        let Some(admin_url) = &self.admin_url else {
            return Err(IdentityError::KratosRejected(
                "session revocation requires the kratos admin API".to_string(),
            ));
        };
        let response = self
            .http
            .delete(format!("{admin_url}/admin/sessions/{session_id}"))
            .send()
            .await
            .map_err(|err| IdentityError::KratosUnreachable(err.to_string()))?;

        match response.status() {
            reqwest::StatusCode::NO_CONTENT
            | reqwest::StatusCode::OK
            | reqwest::StatusCode::NOT_FOUND => Ok(()),
            other => Err(IdentityError::KratosRejected(format!(
                "session revocation returned {other}"
            ))),
        }
    }

    async fn set_metadata_user_id(
        &self,
        kratos_identity_id: uuid::Uuid,
        user_id: UserId,
    ) -> Result<(), IdentityError> {
        let Some(admin_url) = &self.admin_url else {
            return Ok(());
        };
        let response = self
            .http
            .patch(format!("{admin_url}/admin/identities/{kratos_identity_id}"))
            .json(&serde_json::json!({
                "metadata_public": { "user_id": user_id.to_string() }
            }))
            .send()
            .await
            .map_err(|err| IdentityError::KratosUnreachable(err.to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            warn!(
                status = %response.status(),
                "failed to set metadata_public.user_id on kratos identity (best effort)"
            );
            Ok(())
        }
    }

    async fn get_identity(
        &self,
        kratos_identity_id: uuid::Uuid,
    ) -> Result<Option<SessionInfo>, IdentityError> {
        let Some(admin_url) = &self.admin_url else {
            return Ok(None);
        };
        let response = self
            .http
            .get(format!("{admin_url}/admin/identities/{kratos_identity_id}"))
            .send()
            .await
            .map_err(|err| IdentityError::KratosUnreachable(err.to_string()))?;
        match response.status() {
            reqwest::StatusCode::OK => {
                let identity: KratosIdentity = response
                    .json()
                    .await
                    .map_err(|err| IdentityError::MalformedWebhook(err.to_string()))?;
                let email = identity
                    .traits
                    .email
                    .as_deref()
                    .map(Email::parse)
                    .transpose()?
                    .ok_or_else(|| {
                        IdentityError::MalformedWebhook("identity has no email trait".into())
                    })?;
                let email_verified = identity.verifiable_addresses.iter().any(|address| {
                    address.verified
                        && address.status.as_deref() == Some("completed")
                        && address.value.as_deref() == Some(email.as_str())
                });
                let locale = identity
                    .traits
                    .locale
                    .as_deref()
                    .map(Locale::parse)
                    .transpose()
                    .ok()
                    .flatten();
                let oidc_identifiers = identity
                    .credentials
                    .and_then(|credentials| credentials.oidc)
                    .map(|oidc| HttpKratosClient::parse_oidc_identifiers(&oidc.identifiers))
                    .unwrap_or_default();
                Ok(Some(SessionInfo {
                    session_id: String::new(),
                    identity_id: kratos_identity_id,
                    email,
                    email_verified,
                    locale,
                    aal: "aal1".to_string(),
                    methods: Vec::new(),
                    authenticated_at: None,
                    oidc_identifiers,
                }))
            }
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            other => Err(IdentityError::KratosRejected(format!(
                "identity lookup returned {other}"
            ))),
        }
    }

    async fn delete_identity(&self, kratos_identity_id: uuid::Uuid) -> Result<(), IdentityError> {
        let Some(admin_url) = &self.admin_url else {
            // Without the admin API the tombstone is a best-effort no-op;
            // the caller records it in the erasure report.
            return Ok(());
        };
        let response = self
            .http
            .delete(format!("{admin_url}/admin/identities/{kratos_identity_id}"))
            .send()
            .await
            .map_err(|err| IdentityError::KratosUnreachable(err.to_string()))?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT
            | reqwest::StatusCode::OK
            | reqwest::StatusCode::NOT_FOUND => Ok(()),
            other => Err(IdentityError::KratosRejected(format!(
                "identity deletion returned {other}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whoami_fixture() -> serde_json::Value {
        serde_json::json!({
            "id": "session-id-1",
            "active": true,
            "authenticated_at": "2026-08-16T12:00:00Z",
            "identity": {
                "id": "0190f0d0-0000-7000-8000-000000000001",
                "traits": { "email": "Alice@Example.com", "locale": "ja" },
                "verifiable_addresses": [
                    { "value": "alice@example.com", "verified": true, "status": "completed" }
                ],
                "credentials": {
                    "oidc": { "identifiers": ["google:sub-1"] }
                }
            },
            "authentication_methods": [
                { "method": "oidc", "aal": "aal1", "provider": "google" }
            ]
        })
    }

    #[test]
    fn whoami_response_parses_into_session_info() {
        let response: WhoamiResponse = serde_json::from_value(whoami_fixture()).unwrap();
        let session = response.into_session_info().unwrap();
        assert_eq!(session.session_id, "session-id-1");
        assert_eq!(session.email.as_str(), "alice@example.com");
        assert!(session.email_verified);
        assert_eq!(session.locale.as_ref().map(|l| l.as_str()), Some("ja"));
        assert_eq!(session.aal, "aal1");
        assert_eq!(session.methods, vec!["oidc:google"]);
        assert_eq!(session.oidc_identifiers.len(), 1);
        assert_eq!(session.oidc_identifiers[0].0.as_str(), "google");
        assert_eq!(session.oidc_identifiers[0].1.as_str(), "sub-1");
    }

    #[test]
    fn unverified_address_means_unverified() {
        let mut fixture = whoami_fixture();
        fixture["identity"]["verifiable_addresses"] = serde_json::json!([{ "value": "alice@example.com", "verified": false, "status": "completed" }]);
        let response: WhoamiResponse = serde_json::from_value(fixture).unwrap();
        assert!(!response.into_session_info().unwrap().email_verified);
    }

    #[test]
    fn missing_email_trait_is_rejected() {
        let mut fixture = whoami_fixture();
        fixture["identity"]["traits"] = serde_json::json!({});
        let response: WhoamiResponse = serde_json::from_value(fixture).unwrap();
        assert!(matches!(
            response.into_session_info(),
            Err(IdentityError::MalformedWebhook(_))
        ));
    }

    /// A cookie goes back out under the *configured* name. Hardcoding
    /// `ory_kratos_session` here fails silently against a deployment that
    /// renames the cookie: the token is extracted correctly, Kratos does
    /// not recognise the name, and every browser session verifies as 401.
    #[test]
    fn the_cookie_channel_uses_the_configured_cookie_name() {
        let client = HttpKratosClient::new(
            "http://kratos.test".to_string(),
            None,
            "custom_session".to_string(),
        )
        .unwrap();

        let request = client
            .whoami_request("opaque", sanvi_http::SessionChannel::Cookie)
            .build()
            .unwrap();
        assert_eq!(
            request.headers().get(header::COOKIE).unwrap(),
            "custom_session=opaque"
        );
        assert!(request.headers().get("X-Session-Token").is_none());
    }

    /// The bearer channel never sends a cookie: Kratos rejects a request
    /// carrying an API session token in the cookie header.
    #[test]
    fn the_bearer_channel_sends_only_the_session_token_header() {
        let client = HttpKratosClient::new(
            "http://kratos.test".to_string(),
            None,
            "custom_session".to_string(),
        )
        .unwrap();

        let request = client
            .whoami_request("ory_st_opaque", sanvi_http::SessionChannel::Bearer)
            .build()
            .unwrap();
        assert_eq!(
            request.headers().get("X-Session-Token").unwrap(),
            "ory_st_opaque"
        );
        assert!(request.headers().get(header::COOKIE).is_none());
    }
}

#[cfg(test)]
mod credentials_redaction_tests {
    use super::*;
    use crate::adapters::kratos_client_impl::WebhookIdentity;

    /// TASK-024 11.5f regression: Kratos credential material never renders,
    /// directly or through a containing identity Debug.
    #[test]
    fn credentials_never_render() {
        let credentials = Credentials { oidc: None };
        assert_eq!(format!("{credentials:?}"), "Credentials([REDACTED])");
        let identity = WebhookIdentity {
            id: "id_demo".to_string(),
            traits: Default::default(),
            verifiable_addresses: vec![],
            credentials: Some(credentials),
        };
        let debug = format!("{identity:?}");
        assert!(
            debug.contains("Credentials([REDACTED])"),
            "nested render lost the marker: {debug}"
        );
    }
}
