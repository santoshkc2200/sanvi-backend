//! Google Ads API REST adapter (API v22, slice 10.2).
//!
//! Covers:
//! - OAuth 2.0 authorization URL construction (`begin_authorization`)
//! - OAuth authorization-code exchange (`exchange_authorization`)
//! - Account listing against Google Ads API v22 REST surface (`list_accounts`)
//!
//! Campaign management, metrics, conversions, and audiences are scoped for later
//! slices and return `PlatformUnavailable`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::AdvertisingConfig;
use crate::domain::{
    AdPlatformPort, AdvertisingError, AuthorizationExchange, AuthorizationRequest,
    AuthorizationStart, BudgetType, Campaign, CapabilityMatrix, ConnectionAccessTokenProvider,
    ConnectionHandle, ConsentSnapshot, CreativeAssetSpec, CreativePlacement,
    ExchangedAuthorization, GrantedAccessToken, HashedIds, MetricPoint, MetricQuery,
    PlatformAccount, PlatformDescriptor, PlatformKey, RefreshTokenGrant, ScheduleGranularity,
    TargetingDimension, UploadedConversion,
};

const DEFAULT_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_API_BASE: &str = "https://googleads.googleapis.com";
const GOOGLE_ADS_SCOPE: &str = "https://www.googleapis.com/auth/adwords";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Google Ads REST adapter for OAuth lifecycle and account listing.
pub struct GoogleAdsAdapter {
    http: reqwest::Client,
    developer_token: String,
    client_id: String,
    client_secret: String,
    api_version: String,
    auth_base_url: String,
    token_url: String,
    api_base_url: String,
    access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    descriptor: PlatformDescriptor,
}

impl std::fmt::Debug for GoogleAdsAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleAdsAdapter")
            .field("developer_token", &"[REDACTED]")
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .field("api_version", &self.api_version)
            .field("auth_base_url", &self.auth_base_url)
            .field("token_url", &self.token_url)
            .field("api_base_url", &self.api_base_url)
            .finish()
    }
}

#[derive(Serialize)]
struct GoogleAuthQueryParams<'a> {
    client_id: &'a str,
    redirect_uri: &'a str,
    response_type: &'a str,
    scope: &'a str,
    access_type: &'a str,
    prompt: &'a str,
    state: &'a str,
}

#[derive(Serialize)]
struct GoogleRefreshGrantForm<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    refresh_token: &'a str,
    grant_type: &'a str,
}

#[derive(Serialize)]
struct GoogleTokenExchangeForm<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
    grant_type: &'a str,
    redirect_uri: &'a str,
}

#[derive(Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct ListAccessibleCustomersResponse {
    #[serde(default, rename = "resourceNames")]
    resource_names: Vec<String>,
}

impl GoogleAdsAdapter {
    /// Construct a new `GoogleAdsAdapter` from configuration.
    /// Fails cleanly if the developer token is empty.
    pub fn new(
        config: &AdvertisingConfig,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    ) -> Result<Self, AdvertisingError> {
        Self::with_custom_endpoints(
            config,
            DEFAULT_AUTH_URL.to_string(),
            DEFAULT_TOKEN_URL.to_string(),
            DEFAULT_API_BASE.to_string(),
            access_tokens,
        )
    }

    /// Construct with custom endpoint URLs (for tests or mocked transport).
    pub fn with_custom_endpoints(
        config: &AdvertisingConfig,
        auth_base_url: String,
        token_url: String,
        api_base_url: String,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    ) -> Result<Self, AdvertisingError> {
        let developer_token = config.google_ads_developer_token.trim().to_string();
        if developer_token.is_empty() {
            return Err(AdvertisingError::PlatformUnavailable(
                "google ads developer token is required".to_string(),
            ));
        }

        let api_version = if config.google_ads_api_version.trim().is_empty() {
            "v22".to_string()
        } else {
            config.google_ads_api_version.trim().to_string()
        };

        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| AdvertisingError::PlatformUnavailable(err.to_string()))?;

        Ok(Self {
            http,
            developer_token,
            client_id: config.google_ads_client_id.clone(),
            client_secret: config.google_ads_client_secret.clone(),
            api_version,
            auth_base_url,
            token_url,
            api_base_url,
            access_tokens,
            descriptor: google_ads_descriptor(),
        })
    }

    async fn fetch_accessible_customers(
        &self,
        access_token: &str,
    ) -> Result<Vec<PlatformAccount>, AdvertisingError> {
        let url = format!(
            "{}/{}/customers:listAccessibleCustomers",
            self.api_base_url, self.api_version
        );

        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google ads listAccessibleCustomers transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google ads listAccessibleCustomers transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!(
                status = %status,
                "google ads listAccessibleCustomers returned failure"
            );
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google ads customer listing failed with status {status}"
            )));
        }

        let body: ListAccessibleCustomersResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google ads listAccessibleCustomers JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from google ads customer listing".to_string(),
            )
        })?;

        let accounts = body
            .resource_names
            .into_iter()
            .map(|name| {
                let external_id = name.strip_prefix("customers/").unwrap_or(&name).to_string();
                let display_name = format!("Google Ads ({external_id})");
                PlatformAccount {
                    external_id,
                    display_name,
                }
            })
            .collect();

        Ok(accounts)
    }
}

#[async_trait]
impl AdPlatformPort for GoogleAdsAdapter {
    fn descriptor(&self) -> PlatformDescriptor {
        self.descriptor.clone()
    }

    fn required_scopes(&self) -> Vec<String> {
        vec![GOOGLE_ADS_SCOPE.to_string()]
    }

    async fn begin_authorization(
        &self,
        connection: &ConnectionHandle,
        request: &AuthorizationRequest,
    ) -> Result<AuthorizationStart, AdvertisingError> {
        if connection.id.trim().is_empty() || request.redirect_uri.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "authorization requires a connection and redirect URI".to_string(),
            ));
        }
        if request.state.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "authorization requires a non-empty state nonce".to_string(),
            ));
        }

        let params = GoogleAuthQueryParams {
            client_id: &self.client_id,
            redirect_uri: &request.redirect_uri,
            response_type: "code",
            scope: GOOGLE_ADS_SCOPE,
            access_type: "offline",
            prompt: "consent",
            state: &request.state,
        };

        let query = serde_urlencoded::to_string(params).map_err(|err| {
            AdvertisingError::InvalidInput(format!("failed to serialize auth query params: {err}"))
        })?;

        let authorization_url = if self.auth_base_url.contains('?') {
            format!("{}&{}", self.auth_base_url, query)
        } else {
            format!("{}?{}", self.auth_base_url, query)
        };

        Ok(AuthorizationStart { authorization_url })
    }

    async fn exchange_authorization(
        &self,
        exchange: &AuthorizationExchange,
    ) -> Result<ExchangedAuthorization, AdvertisingError> {
        if exchange.code.trim().is_empty() || exchange.redirect_uri.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "authorization exchange requires a code and redirect URI".to_string(),
            ));
        }

        let form = GoogleTokenExchangeForm {
            client_id: &self.client_id,
            client_secret: &self.client_secret,
            code: &exchange.code,
            grant_type: "authorization_code",
            redirect_uri: &exchange.redirect_uri,
        };

        let response = self
            .http
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google oauth token request failed");
                AdvertisingError::PlatformUnavailable(
                    "google oauth token request failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!(status = %status, "google oauth token endpoint returned non-success");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google oauth token exchange failed with status {status}"
            )));
        }

        let token_resp: GoogleTokenResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google oauth token JSON parsing failed");
            AdvertisingError::PlatformUnavailable("invalid token response from google".to_string())
        })?;

        if let Some(err) = token_resp.error {
            let desc = token_resp.error_description.unwrap_or_default();
            tracing::warn!(error = %err, description = %desc, "google oauth reported token error");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google oauth returned error: {err}"
            )));
        }

        let refresh_token = token_resp.refresh_token.ok_or_else(|| {
            AdvertisingError::PlatformUnavailable(
                "google oauth did not return a refresh token".to_string(),
            )
        })?;

        let granted_scopes = token_resp
            .scope
            .map(|s| {
                s.split_whitespace()
                    .map(|scope| scope.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let expires_at = token_resp
            .expires_in
            .map(|secs| Utc::now() + chrono::Duration::seconds(secs));

        let accounts = self
            .fetch_accessible_customers(&token_resp.access_token)
            .await?;

        Ok(ExchangedAuthorization {
            refresh_token,
            granted_scopes,
            expires_at,
            accounts,
        })
    }

    async fn list_accounts(
        &self,
        connection: &ConnectionHandle,
    ) -> Result<Vec<PlatformAccount>, AdvertisingError> {
        if connection.id.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "connection id is required to list accounts".to_string(),
            ));
        }

        // `connection.id` is a `ConnectionId`, never a credential — the bearer
        // token comes from the provider that owns the vault lookup and refresh.
        let access_token = self.access_tokens.access_token(connection).await?;
        self.fetch_accessible_customers(&access_token).await
    }

    async fn create_campaign(
        &self,
        _connection: &ConnectionHandle,
        _campaign: Campaign,
        _mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: create_campaign not yet implemented".to_string(),
        ))
    }

    async fn get_campaign(
        &self,
        _connection: &ConnectionHandle,
        _campaign_id: &str,
    ) -> Result<Option<Campaign>, AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: get_campaign not yet implemented".to_string(),
        ))
    }

    async fn update_campaign(
        &self,
        _connection: &ConnectionHandle,
        _campaign: Campaign,
        _mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: update_campaign not yet implemented".to_string(),
        ))
    }

    async fn pause_campaign(
        &self,
        _connection: &ConnectionHandle,
        _campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: pause_campaign not yet implemented".to_string(),
        ))
    }

    async fn resume_campaign(
        &self,
        _connection: &ConnectionHandle,
        _campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: resume_campaign not yet implemented".to_string(),
        ))
    }

    async fn delete_campaign(
        &self,
        _connection: &ConnectionHandle,
        _campaign_id: &str,
    ) -> Result<(), AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: delete_campaign not yet implemented".to_string(),
        ))
    }

    async fn fetch_metrics(
        &self,
        _connection: &ConnectionHandle,
        _request: &MetricQuery,
    ) -> Result<Vec<MetricPoint>, AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: fetch_metrics not yet implemented".to_string(),
        ))
    }

    async fn upload(
        &self,
        _connection: &ConnectionHandle,
        _conversion: UploadedConversion,
        _ids: HashedIds,
        _consent: ConsentSnapshot,
    ) -> Result<(), AdvertisingError> {
        Err(AdvertisingError::PlatformUnavailable(
            "google adapter: upload not yet implemented".to_string(),
        ))
    }
}

fn google_ads_descriptor() -> PlatformDescriptor {
    let objective_set = ["app_promotion", "awareness", "leads", "sales", "traffic"]
        .iter()
        .map(|value| crate::domain::Objective::parse(value).expect("static objective"))
        .collect::<BTreeSet<_>>();
    let dimensions = [
        "audiences",
        "demographics",
        "devices",
        "geo",
        "keywords",
        "topics",
    ]
    .iter()
    .map(|value| TargetingDimension::parse(value).expect("static dimension"))
    .collect::<BTreeSet<_>>();
    let budget_types = [BudgetType::Daily, BudgetType::Lifetime]
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();

    let mut minimums = BTreeMap::new();
    minimums.insert(
        "JPY".to_string(),
        budget_types.iter().map(|kind| (*kind, 100)).collect(),
    );

    let mut text_limits = BTreeMap::new();
    text_limits.insert(
        "en".to_string(),
        BTreeMap::from([("headline".to_string(), 30), ("body".to_string(), 90)]),
    );
    text_limits.insert(
        "ja".to_string(),
        BTreeMap::from([("headline".to_string(), 30), ("body".to_string(), 45)]),
    );

    PlatformDescriptor {
        key: PlatformKey::parse("google_ads").expect("static key"),
        display_name: "Google Ads".to_string(),
        entitlement_key: "advertising.google_ads".to_string(),
        capability_matrix: CapabilityMatrix {
            matrix_version: "2026-08-24".to_string(),
            objectives: objective_set,
            budget_types,
            minimum_budgets_minor: minimums,
            schedule_granularity: ScheduleGranularity::Daily,
            targeting_dimensions: dimensions,
            creative_placements: vec![
                CreativePlacement {
                    key: "feed".to_string(),
                    asset_spec: CreativeAssetSpec {
                        image_required: true,
                        video_allowed: true,
                        aspect_ratios: vec!["1:1".to_string(), "4:5".to_string()],
                        min_width_px: Some(300),
                        min_height_px: Some(300),
                        max_file_size_bytes: Some(5_242_880),
                        min_duration_seconds: Some(5),
                        max_duration_seconds: Some(180),
                    },
                },
                CreativePlacement {
                    key: "stories".to_string(),
                    asset_spec: CreativeAssetSpec {
                        image_required: true,
                        video_allowed: true,
                        aspect_ratios: vec!["9:16".to_string()],
                        min_width_px: Some(480),
                        min_height_px: Some(854),
                        max_file_size_bytes: Some(5_242_880),
                        min_duration_seconds: Some(5),
                        max_duration_seconds: Some(60),
                    },
                },
            ],
            text_limits,
        },
    }
}

/// Google's refresh-token grant, the credential half of
/// [`crate::application::VaultAccessTokenProvider`]. Deliberately separate from
/// `GoogleAdsAdapter`: this is the only place a decrypted refresh token is
/// handled, and it must not become reachable through `AdPlatformPort`.
pub struct GoogleRefreshTokenGrant {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    token_url: String,
}

impl std::fmt::Debug for GoogleRefreshTokenGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleRefreshTokenGrant")
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .field("token_url", &self.token_url)
            .finish()
    }
}

impl GoogleRefreshTokenGrant {
    pub fn new(config: &AdvertisingConfig) -> Result<Self, AdvertisingError> {
        Self::with_token_url(config, DEFAULT_TOKEN_URL.to_string())
    }

    pub fn with_token_url(
        config: &AdvertisingConfig,
        token_url: String,
    ) -> Result<Self, AdvertisingError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| AdvertisingError::PlatformUnavailable(err.to_string()))?;
        Ok(Self {
            http,
            client_id: config.google_ads_client_id.clone(),
            client_secret: config.google_ads_client_secret.clone(),
            token_url,
        })
    }
}

#[async_trait]
impl RefreshTokenGrant for GoogleRefreshTokenGrant {
    async fn exchange_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<GrantedAccessToken, AdvertisingError> {
        if refresh_token.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "refresh token is required".to_string(),
            ));
        }

        let form = GoogleRefreshGrantForm {
            client_id: &self.client_id,
            client_secret: &self.client_secret,
            refresh_token,
            grant_type: "refresh_token",
        };

        let response = self
            .http
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google oauth refresh grant transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google oauth refresh grant failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!(status = %status, "google oauth refresh grant returned non-success");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google oauth refresh grant failed with status {status}"
            )));
        }

        let token_resp: GoogleTokenResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google oauth refresh grant JSON parsing failed");
            AdvertisingError::PlatformUnavailable(
                "invalid refresh grant response from google".to_string(),
            )
        })?;

        if let Some(err) = token_resp.error {
            let desc = token_resp.error_description.unwrap_or_default();
            tracing::warn!(error = %err, description = %desc, "google oauth refresh grant error");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google oauth refresh grant returned error: {err}"
            )));
        }

        Ok(GrantedAccessToken {
            access_token: token_resp.access_token,
            expires_at: token_resp
                .expires_in
                .map(|secs| Utc::now() + chrono::Duration::seconds(secs)),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        developer_token: Option<String>,
        body: Vec<u8>,
    }

    struct MockServer {
        pub addr: std::net::SocketAddr,
        recorded: Arc<Mutex<Vec<RecordedRequest>>>,
        shutdown: tokio::sync::oneshot::Sender<()>,
    }

    impl MockServer {
        async fn start(responses: Vec<(u16, serde_json::Value)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let recorded = Arc::new(Mutex::new(Vec::new()));
            let recorded_cloned = recorded.clone();
            let (shutdown, mut rx) = tokio::sync::oneshot::channel::<()>();

            tokio::spawn(async move {
                let mut response_iter = responses.into_iter();
                loop {
                    tokio::select! {
                        _ = &mut rx => break,
                        accept_res = listener.accept() => {
                            let Ok((mut socket, _)) = accept_res else {
                                break;
                            };
                            let (status, resp_json) = response_iter.next().unwrap_or((404, serde_json::json!({})));
                            let mut buf = vec![0u8; 4096];
                            let n = socket.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                continue;
                            }
                            let head = String::from_utf8_lossy(&buf[..n]).to_string();
                            let mut lines = head.split("\r\n");
                            let req_line = lines.next().unwrap_or("");
                            let parts: Vec<&str> = req_line.split_whitespace().collect();
                            let method = parts.first().unwrap_or(&"").to_string();
                            let path = parts.get(1).unwrap_or(&"").to_string();

                            let mut authorization = None;
                            let mut developer_token = None;
                            let mut content_length = 0usize;
                            for line in lines {
                                if let Some((k, v)) = line.split_once(':') {
                                    let k = k.trim().to_ascii_lowercase();
                                    let v = v.trim().to_string();
                                    if k == "authorization" {
                                        authorization = Some(v);
                                    } else if k == "developer-token" {
                                        developer_token = Some(v);
                                    } else if k == "content-length" {
                                        content_length = v.parse().unwrap_or(0);
                                    }
                                }
                            }

                            let body_start = head.find("\r\n\r\n").map(|p| p + 4).unwrap_or(n);
                            let body = buf[body_start..n.min(body_start + content_length)].to_vec();

                            recorded_cloned.lock().unwrap().push(RecordedRequest {
                                method,
                                path,
                                authorization,
                                developer_token,
                                body,
                            });

                            let resp_body = serde_json::to_string(&resp_json).unwrap();
                            let http_response = format!(
                                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                                resp_body.len()
                            );
                            let _ = socket.write_all(http_response.as_bytes()).await;
                        }
                    }
                }
            });

            Self {
                addr,
                recorded,
                shutdown,
            }
        }

        fn recorded(&self) -> Vec<RecordedRequest> {
            self.recorded.lock().unwrap().clone()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            let (shutdown, _) = tokio::sync::oneshot::channel();
            let old = std::mem::replace(&mut self.shutdown, shutdown);
            let _ = old.send(());
        }
    }

    fn test_tenant() -> sanvi_kernel::TenantId {
        sanvi_kernel::TenantId::try_from(1_i64).unwrap()
    }

    fn test_config() -> AdvertisingConfig {
        AdvertisingConfig {
            google_ads_developer_token: "test_dev_token_123".to_string(),
            google_ads_client_id: "test_client_id_456".to_string(),
            google_ads_client_secret: "test_client_secret_789".to_string(),
            google_ads_api_version: "v22".to_string(),
            ..AdvertisingConfig::default()
        }
    }

    /// Stands in for the vault-backed provider: it hands back a token that is
    /// deliberately unrelated to the connection id, so a regression that reuses
    /// the handle as a credential fails the header assertion.
    struct StubAccessTokens {
        token: String,
    }

    #[async_trait]
    impl ConnectionAccessTokenProvider for StubAccessTokens {
        async fn access_token(
            &self,
            _connection: &ConnectionHandle,
        ) -> Result<String, AdvertisingError> {
            Ok(self.token.clone())
        }
    }

    fn stub_tokens(token: &str) -> Arc<dyn ConnectionAccessTokenProvider> {
        Arc::new(StubAccessTokens {
            token: token.to_string(),
        })
    }

    #[test]
    fn construction_fails_cleanly_on_empty_developer_token() {
        let config = AdvertisingConfig {
            google_ads_developer_token: "   ".to_string(),
            ..test_config()
        };
        let err = GoogleAdsAdapter::new(&config, stub_tokens("ya29.unused"))
            .expect_err("empty developer token must fail");
        assert!(matches!(err, AdvertisingError::PlatformUnavailable(_)));
    }

    #[test]
    fn debug_output_redacts_credentials() {
        let adapter = GoogleAdsAdapter::new(&test_config(), stub_tokens("ya29.unused")).unwrap();
        let debug = format!("{adapter:?}");
        assert!(!debug.contains("test_dev_token_123"));
        assert!(!debug.contains("test_client_id_456"));
        assert!(!debug.contains("test_client_secret_789"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn descriptor_and_scopes_are_correct() {
        let adapter = GoogleAdsAdapter::new(&test_config(), stub_tokens("ya29.unused")).unwrap();
        assert_eq!(adapter.descriptor().key.as_str(), "google_ads");
        assert_eq!(adapter.required_scopes(), vec![GOOGLE_ADS_SCOPE]);
    }

    #[tokio::test]
    async fn begin_authorization_constructs_valid_google_oauth_url() {
        let adapter = GoogleAdsAdapter::new(&test_config(), stub_tokens("ya29.unused")).unwrap();
        let connection = ConnectionHandle {
            id: "conn-123".to_string(),
            tenant_id: test_tenant(),
        };
        let request = AuthorizationRequest {
            redirect_uri: "https://tenant.example/callback".to_string(),
            state: "test-nonce-state".to_string(),
        };

        let start = adapter
            .begin_authorization(&connection, &request)
            .await
            .expect("authorization starts");
        assert!(start.authorization_url.starts_with(DEFAULT_AUTH_URL));
        assert!(
            start
                .authorization_url
                .contains("client_id=test_client_id_456")
        );
        assert!(start.authorization_url.contains("response_type=code"));
        assert!(start.authorization_url.contains("access_type=offline"));
        assert!(start.authorization_url.contains("prompt=consent"));
        assert!(start.authorization_url.contains("state=test-nonce-state"));
    }

    #[tokio::test]
    async fn exchange_authorization_redeems_code_and_lists_accounts() {
        let token_fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/google/token_response.json"
        ))
        .unwrap();
        let customers_fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/google/list_accessible_customers_response.json"
        ))
        .unwrap();

        let mock = MockServer::start(vec![(200, token_fixture), (200, customers_fixture)]).await;

        let adapter = GoogleAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}/token", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();

        let exchange = AuthorizationExchange {
            code: "auth-code-xyz".to_string(),
            redirect_uri: "https://tenant.example/callback".to_string(),
        };

        let res = adapter
            .exchange_authorization(&exchange)
            .await
            .expect("exchange succeeds");

        assert_eq!(
            res.refresh_token,
            "1//04_sample_google_ads_refresh_token_v22"
        );
        assert_eq!(res.granted_scopes, vec![GOOGLE_ADS_SCOPE]);
        assert_eq!(res.accounts.len(), 2);
        assert_eq!(res.accounts[0].external_id, "1234567890");
        assert_eq!(res.accounts[1].external_id, "9876543210");

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 2);
        // Token call
        assert_eq!(recorded[0].method, "POST");
        assert_eq!(recorded[0].path, "/token");
        let body_str = String::from_utf8_lossy(&recorded[0].body);
        assert!(body_str.contains("code=auth-code-xyz"));
        assert!(body_str.contains("grant_type=authorization_code"));

        // Customers call
        assert_eq!(recorded[1].method, "GET");
        assert_eq!(recorded[1].path, "/v22/customers:listAccessibleCustomers");
        assert_eq!(
            recorded[1].authorization.as_deref(),
            Some("Bearer ya29.sample_google_ads_access_token_v22")
        );
        assert_eq!(
            recorded[1].developer_token.as_deref(),
            Some("test_dev_token_123")
        );
    }

    #[tokio::test]
    async fn list_accounts_queries_google_ads_v22_rest_api() {
        let customers_fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/google/list_accessible_customers_response.json"
        ))
        .unwrap();

        let mock = MockServer::start(vec![(200, customers_fixture)]).await;

        let adapter = GoogleAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}/token", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();

        // A real caller passes a ConnectionId, never a credential.
        let connection = ConnectionHandle {
            id: "7318492050313216".to_string(),
            tenant_id: test_tenant(),
        };

        let accounts = adapter
            .list_accounts(&connection)
            .await
            .expect("list accounts succeeds");
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].external_id, "1234567890");

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].method, "GET");
        assert_eq!(recorded[0].path, "/v22/customers:listAccessibleCustomers");
        assert_eq!(
            recorded[0].authorization.as_deref(),
            Some("Bearer ya29.provider_issued_access_token")
        );
        assert_eq!(
            recorded[0].developer_token.as_deref(),
            Some("test_dev_token_123")
        );
    }

    #[tokio::test]
    async fn refresh_grant_trades_refresh_token_for_access_token() {
        let token_fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/google/token_response.json"
        ))
        .unwrap();
        let mock = MockServer::start(vec![(200, token_fixture)]).await;

        let grant = GoogleRefreshTokenGrant::with_token_url(
            &test_config(),
            format!("http://{}/token", mock.addr),
        )
        .unwrap();

        let granted = grant
            .exchange_refresh_token("1//04_stored_refresh_token")
            .await
            .expect("refresh grant succeeds");
        assert_eq!(
            granted.access_token,
            "ya29.sample_google_ads_access_token_v22"
        );
        assert!(granted.expires_at.is_some());

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        let body = String::from_utf8_lossy(&recorded[0].body);
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=1%2F%2F04_stored_refresh_token"));
    }

    #[test]
    fn refresh_grant_debug_redacts_credentials() {
        let grant = GoogleRefreshTokenGrant::new(&test_config()).unwrap();
        let debug = format!("{grant:?}");
        assert!(!debug.contains("test_client_id_456"));
        assert!(!debug.contains("test_client_secret_789"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn refresh_grant_rejects_an_empty_refresh_token_without_a_call() {
        let mock = MockServer::start(vec![]).await;
        let grant = GoogleRefreshTokenGrant::with_token_url(
            &test_config(),
            format!("http://{}/token", mock.addr),
        )
        .unwrap();

        let err = grant
            .exchange_refresh_token("   ")
            .await
            .expect_err("empty refresh token is rejected");
        assert!(matches!(err, AdvertisingError::InvalidInput(_)));
        assert!(mock.recorded().is_empty());
    }

    #[tokio::test]
    async fn unimplemented_methods_return_honest_platform_unavailable() {
        let adapter = GoogleAdsAdapter::new(&test_config(), stub_tokens("ya29.unused")).unwrap();
        let connection = ConnectionHandle {
            id: "conn-1".to_string(),
            tenant_id: test_tenant(),
        };

        let create_err = adapter
            .create_campaign(
                &connection,
                crate::domain::Campaign {
                    id: "c-1".to_string(),
                    name: "Test".to_string(),
                    status: crate::domain::CampaignStatus::Draft,
                    objective: crate::domain::Objective::parse("sales").unwrap(),
                    budget: crate::domain::Budget {
                        kind: BudgetType::Daily,
                        amount: crate::domain::Money::new(100, "JPY").unwrap(),
                    },
                    schedule: None,
                    ad_groups: vec![],
                    drift: crate::domain::DriftState {
                        drifted: false,
                        changed_fields: vec![],
                    },
                },
                Uuid::now_v7(),
            )
            .await
            .expect_err("unimplemented");
        assert_eq!(
            create_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: create_campaign not yet implemented".to_string()
            )
        );

        let get_err = adapter
            .get_campaign(&connection, "c-1")
            .await
            .expect_err("unimplemented");
        assert_eq!(
            get_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: get_campaign not yet implemented".to_string()
            )
        );

        let update_err = adapter
            .update_campaign(
                &connection,
                crate::domain::Campaign {
                    id: "c-1".to_string(),
                    name: "Test".to_string(),
                    status: crate::domain::CampaignStatus::Draft,
                    objective: crate::domain::Objective::parse("sales").unwrap(),
                    budget: crate::domain::Budget {
                        kind: BudgetType::Daily,
                        amount: crate::domain::Money::new(100, "JPY").unwrap(),
                    },
                    schedule: None,
                    ad_groups: vec![],
                    drift: crate::domain::DriftState {
                        drifted: false,
                        changed_fields: vec![],
                    },
                },
                Uuid::now_v7(),
            )
            .await
            .expect_err("unimplemented");
        assert_eq!(
            update_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: update_campaign not yet implemented".to_string()
            )
        );

        let pause_err = adapter
            .pause_campaign(&connection, "c-1")
            .await
            .expect_err("unimplemented");
        assert_eq!(
            pause_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: pause_campaign not yet implemented".to_string()
            )
        );

        let resume_err = adapter
            .resume_campaign(&connection, "c-1")
            .await
            .expect_err("unimplemented");
        assert_eq!(
            resume_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: resume_campaign not yet implemented".to_string()
            )
        );

        let delete_err = adapter
            .delete_campaign(&connection, "c-1")
            .await
            .expect_err("unimplemented");
        assert_eq!(
            delete_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: delete_campaign not yet implemented".to_string()
            )
        );

        let metrics_err = adapter
            .fetch_metrics(
                &connection,
                &MetricQuery {
                    from: "2026-08-01".to_string(),
                    to: "2026-08-02".to_string(),
                },
            )
            .await
            .expect_err("unimplemented");
        assert_eq!(
            metrics_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: fetch_metrics not yet implemented".to_string()
            )
        );

        let upload_err = adapter
            .upload(
                &connection,
                UploadedConversion {
                    event_id: "evt-1".to_string(),
                },
                HashedIds::default(),
                ConsentSnapshot {
                    resolver_version: "v1".to_string(),
                    jurisdiction: "US-CA".to_string(),
                    purposes_asked: vec![],
                    answers: BTreeMap::new(),
                    signal_source: "browser".to_string(),
                },
            )
            .await
            .expect_err("unimplemented");
        assert_eq!(
            upload_err,
            AdvertisingError::PlatformUnavailable(
                "google adapter: upload not yet implemented".to_string()
            )
        );
    }
}
