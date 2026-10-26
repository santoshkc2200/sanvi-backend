//! freee OAuth client and read-only accounting lookups.
//!
//! Endpoints and fields follow freee's start guide and its published OpenAPI
//! schema (`freee/freee-api-schema`, `v2020_06_15`). Consent always uses
//! `prompt=select_company`, which limits the grant to one company and makes
//! the token response report its `company_id`.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use serde::Deserialize;

use crate::{
    application::{ProviderDirectory, ProviderOAuth},
    domain::{AccountingError, Provider, ProviderCatalog, Secret, TokenGrant},
};

pub const AUTHORIZE_URL: &str = "https://accounts.secure.freee.co.jp/public_api/authorize";
pub const TOKEN_URL: &str = "https://accounts.secure.freee.co.jp/public_api/token";
/// Known only from community sample code; revocation is best-effort.
pub const REVOKE_URL: &str = "https://accounts.secure.freee.co.jp/public_api/revoke";
pub const API_BASE_URL: &str = "https://api.freee.co.jp";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Refresh runs under a database row lock, so a slow provider must not hold it long.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// freee company roles that may register deals and journals.
const POSTING_ROLES: [&str; 2] = ["admin", "simple_accounting"];

#[derive(Clone)]
pub struct FreeeSettings {
    pub client_id: String,
    pub client_secret: Secret,
    /// Must equal the redirect URI registered on the freee app.
    pub redirect_uri: String,
    pub authorize_url: String,
    pub token_url: String,
    pub revoke_url: String,
    pub api_base_url: String,
}

impl FreeeSettings {
    pub fn production(client_id: String, client_secret: Secret, redirect_uri: String) -> Self {
        Self {
            client_id,
            client_secret,
            redirect_uri,
            authorize_url: AUTHORIZE_URL.into(),
            token_url: TOKEN_URL.into(),
            revoke_url: REVOKE_URL.into(),
            api_base_url: API_BASE_URL.into(),
        }
    }
}

pub struct FreeeClient {
    http: reqwest::Client,
    settings: FreeeSettings,
}

impl FreeeClient {
    pub fn new(settings: FreeeSettings) -> Result<Self, AccountingError> {
        if settings.client_id.trim().is_empty()
            || settings.client_secret.expose().trim().is_empty()
            || settings.redirect_uri.trim().is_empty()
        {
            return Err(AccountingError::VerificationUnavailable);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AccountingError::VerificationUnavailable)?;
        Ok(Self { http, settings })
    }

    async fn token(&self, form: &[(&str, &str)]) -> Result<TokenGrant, AccountingError> {
        let response = self
            .http
            .post(&self.settings.token_url)
            .form(form)
            .send()
            .await
            .map_err(|_| AccountingError::ProviderUnavailable)?;
        let status = response.status();
        if status.is_success() {
            let body: TokenResponse = response
                .json()
                .await
                .map_err(|_| AccountingError::ProviderUnavailable)?;
            return Ok(TokenGrant {
                access_token: Secret::new(body.access_token),
                refresh_token: Secret::new(body.refresh_token),
                access_expires_at: Utc::now() + TimeDelta::seconds(body.expires_in),
                organization_id: body.company_id.map(|id| id.to_string()),
            });
        }
        let error = response
            .json::<TokenError>()
            .await
            .map(|body| body.error)
            .unwrap_or_default();
        Err(match (status.as_u16(), error.as_str()) {
            (_, "invalid_grant") => AccountingError::ReauthorizationRequired,
            (429 | 500..=599, _) => AccountingError::ProviderUnavailable,
            _ => AccountingError::AuthorizationRejected("freee rejected the token request"),
        })
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        access_token: &Secret,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, AccountingError> {
        let response = self
            .http
            .get(format!("{}{path}", self.settings.api_base_url))
            .bearer_auth(access_token.expose())
            .query(query)
            .send()
            .await
            .map_err(|_| AccountingError::ProviderUnavailable)?;
        match response.status().as_u16() {
            200..=299 => response
                .json()
                .await
                .map_err(|_| AccountingError::ProviderUnavailable),
            401 => Err(AccountingError::ReauthorizationRequired),
            429 | 500..=599 => Err(AccountingError::ProviderUnavailable),
            _ => Err(AccountingError::VerificationFailed(
                "freee denied access to the organization",
            )),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    company_id: Option<i64>,
}

#[derive(Deserialize, Default)]
struct TokenError {
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct CompanyResponse {
    company: Company,
}
#[derive(Deserialize)]
struct Company {
    role: String,
}

#[derive(Deserialize)]
struct AccountItemsResponse {
    account_items: Vec<AccountItem>,
}
#[derive(Deserialize)]
struct AccountItem {
    id: i64,
    available: bool,
}

#[derive(Deserialize)]
struct TaxesResponse {
    taxes: Vec<Tax>,
}
#[derive(Deserialize)]
struct Tax {
    code: i64,
    available: bool,
}

#[async_trait]
impl ProviderOAuth for FreeeClient {
    fn provider(&self) -> Provider {
        Provider::Freee
    }

    fn authorization_url(&self, state: &str) -> String {
        let query = serde_urlencoded::to_string([
            ("response_type", "code"),
            ("client_id", self.settings.client_id.as_str()),
            ("redirect_uri", self.settings.redirect_uri.as_str()),
            ("state", state),
            ("prompt", "select_company"),
        ])
        .expect("string pairs encode");
        format!("{}?{query}", self.settings.authorize_url)
    }

    async fn exchange_code(&self, code: &str) -> Result<TokenGrant, AccountingError> {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("client_id", &self.settings.client_id),
            ("client_secret", self.settings.client_secret.expose()),
            ("code", code),
            ("redirect_uri", &self.settings.redirect_uri),
        ])
        .await
    }

    async fn refresh(&self, refresh_token: &Secret) -> Result<TokenGrant, AccountingError> {
        self.token(&[
            ("grant_type", "refresh_token"),
            ("client_id", &self.settings.client_id),
            ("client_secret", self.settings.client_secret.expose()),
            ("refresh_token", refresh_token.expose()),
            ("redirect_uri", &self.settings.redirect_uri),
        ])
        .await
    }

    async fn revoke(&self, refresh_token: &Secret) -> Result<(), AccountingError> {
        let response = self
            .http
            .post(&self.settings.revoke_url)
            .form(&[
                ("client_id", self.settings.client_id.as_str()),
                ("client_secret", self.settings.client_secret.expose()),
                ("token", refresh_token.expose()),
            ])
            .send()
            .await
            .map_err(|_| AccountingError::ProviderUnavailable)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(AccountingError::ProviderUnavailable)
        }
    }
}

#[async_trait]
impl ProviderDirectory for FreeeClient {
    fn provider(&self) -> Provider {
        Provider::Freee
    }

    async fn catalog(
        &self,
        access_token: &Secret,
        organization_id: &str,
    ) -> Result<ProviderCatalog, AccountingError> {
        let company_id: i64 = organization_id.parse().ok().filter(|id| *id > 0).ok_or(
            AccountingError::VerificationFailed(
                "freee organization reference must be a company ID",
            ),
        )?;
        let company_id = company_id.to_string();
        let company: CompanyResponse = self
            .get(access_token, &format!("/api/1/companies/{company_id}"), &[])
            .await?;
        let accounts: AccountItemsResponse = self
            .get(
                access_token,
                "/api/1/account_items",
                &[("company_id", &company_id)],
            )
            .await?;
        let taxes: TaxesResponse = self
            .get(
                access_token,
                &format!("/api/1/taxes/companies/{company_id}"),
                &[("available", "true")],
            )
            .await?;
        Ok(ProviderCatalog {
            can_post: POSTING_ROLES.contains(&company.company.role.as_str()),
            account_ids: accounts
                .account_items
                .into_iter()
                .filter(|item| item.available)
                .map(|item| item.id.to_string())
                .collect(),
            tax_codes: taxes
                .taxes
                .into_iter()
                .filter(|tax| tax.available)
                .map(|tax| tax.code.to_string())
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// Serves canned responses in order and records each request line and body.
    async fn serve(responses: Vec<(u16, serde_json::Value)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 16 * 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = request.lines().next().unwrap_or_default().to_string();
                let body_in = request.split("\r\n\r\n").nth(1).unwrap_or_default();
                record.lock().unwrap().push(format!("{line} {body_in}"));
                let body = body.to_string();
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        (base, seen)
    }

    fn client(base: &str) -> FreeeClient {
        FreeeClient::new(FreeeSettings {
            client_id: "app".into(),
            client_secret: Secret::new("app-secret"),
            redirect_uri: "https://admin.example.test/accounting/callback".into(),
            authorize_url: format!("{base}/authorize"),
            token_url: format!("{base}/token"),
            revoke_url: format!("{base}/revoke"),
            api_base_url: base.into(),
        })
        .unwrap()
    }

    #[test]
    fn consent_url_selects_one_company_and_carries_state() {
        let url = client("https://accounts.example").authorization_url("v1.a.b");
        assert!(url.starts_with("https://accounts.example/authorize?response_type=code&"));
        assert!(url.contains("state=v1.a.b"));
        assert!(url.contains("prompt=select_company"));
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fadmin.example.test%2Faccounting%2Fcallback")
        );
    }

    #[test]
    fn missing_app_credentials_fail_closed() {
        let mut settings = FreeeSettings::production(
            "app".into(),
            Secret::new(" "),
            "https://admin.example.test/cb".into(),
        );
        assert!(matches!(
            FreeeClient::new(settings.clone()),
            Err(AccountingError::VerificationUnavailable)
        ));
        settings.client_secret = Secret::new("secret");
        assert!(FreeeClient::new(settings).is_ok());
    }

    #[tokio::test]
    async fn code_exchange_reports_the_selected_company() {
        let (base, seen) = serve(vec![(
            200,
            serde_json::json!({
                "access_token": "at", "refresh_token": "rt", "expires_in": 21600,
                "token_type": "bearer", "company_id": 42, "external_cid": "x"
            }),
        )])
        .await;
        let grant = client(&base).exchange_code("the-code").await.unwrap();
        assert_eq!(grant.access_token.expose(), "at");
        assert_eq!(grant.refresh_token.expose(), "rt");
        assert_eq!(grant.organization_id.as_deref(), Some("42"));
        assert!(grant.access_expires_at > Utc::now() + TimeDelta::hours(5));
        let request = &seen.lock().unwrap()[0];
        assert!(request.starts_with("POST /token "));
        assert!(request.contains("grant_type=authorization_code"));
        assert!(request.contains("code=the-code"));
    }

    #[tokio::test]
    async fn refused_refresh_requires_new_consent_and_outages_do_not() {
        let (base, _) = serve(vec![
            (401, serde_json::json!({"error": "invalid_grant"})),
            (503, serde_json::json!({})),
            (400, serde_json::json!({"error": "invalid_client"})),
        ])
        .await;
        let client = client(&base);
        let token = Secret::new("rt");
        assert_eq!(
            client.refresh(&token).await.unwrap_err(),
            AccountingError::ReauthorizationRequired
        );
        assert_eq!(
            client.refresh(&token).await.unwrap_err(),
            AccountingError::ProviderUnavailable
        );
        assert!(matches!(
            client.refresh(&token).await.unwrap_err(),
            AccountingError::AuthorizationRejected(_)
        ));
    }

    #[tokio::test]
    async fn catalog_keeps_only_available_accounts_and_tax_codes() {
        let (base, seen) = serve(vec![
            (
                200,
                serde_json::json!({"company": {"id": 42, "role": "admin"}}),
            ),
            (
                200,
                serde_json::json!({"account_items": [
                    {"id": 101, "available": true}, {"id": 102, "available": false}
                ]}),
            ),
            (
                200,
                serde_json::json!({"taxes": [
                    {"code": 129, "available": true}, {"code": 2, "available": false}
                ]}),
            ),
        ])
        .await;
        let catalog = client(&base)
            .catalog(&Secret::new("at"), "42")
            .await
            .unwrap();
        assert!(catalog.can_post);
        assert_eq!(catalog.account_ids, ["101".to_string()].into());
        assert_eq!(catalog.tax_codes, ["129".to_string()].into());
        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with("GET /api/1/companies/42 "));
        assert!(seen[1].starts_with("GET /api/1/account_items?company_id=42 "));
        assert!(seen[2].starts_with("GET /api/1/taxes/companies/42?available=true "));
    }

    #[tokio::test]
    async fn read_only_role_cannot_post_and_bad_company_reference_is_rejected() {
        let (base, _) = serve(vec![
            (200, serde_json::json!({"company": {"role": "read_only"}})),
            (200, serde_json::json!({"account_items": []})),
            (200, serde_json::json!({"taxes": []})),
        ])
        .await;
        let client = client(&base);
        assert!(
            !client
                .catalog(&Secret::new("at"), "42")
                .await
                .unwrap()
                .can_post
        );
        assert!(matches!(
            client.catalog(&Secret::new("at"), "acme").await,
            Err(AccountingError::VerificationFailed(_))
        ));
    }
}
