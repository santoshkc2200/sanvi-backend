//! The Kratos REST adapter: session verification, revocation and identity
//! metadata. Pins no SDK — the surface we use is small and stable (public
//! `whoami` + session revocation, admin identity PATCH).

use async_trait::async_trait;
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
}

impl HttpKratosClient {
    pub fn new(public_url: String, admin_url: Option<String>) -> Result<Self, IdentityError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|err| IdentityError::KratosUnreachable(err.to_string()))?;
        Ok(Self {
            http,
            public_url,
            admin_url,
        })
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

#[derive(Debug, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    oidc: Option<OidcCredentials>,
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
            .authentication_methods
            .iter()
            .find_map(|method| method.aal.clone())
            .unwrap_or_else(|| "aal1".to_string());

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
    async fn whoami(&self, session_token: &str) -> Result<Option<SessionInfo>, IdentityError> {
        let response = self
            .http
            .get(format!("{}/sessions/whoami", self.public_url))
            .header("X-Session-Token", session_token)
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
}
