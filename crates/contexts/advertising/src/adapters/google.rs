//! Google Ads API REST adapter (API v22, slice 10.2).
//!
//! Covers:
//! - OAuth 2.0 authorization URL construction (`begin_authorization`)
//! - OAuth authorization-code exchange (`exchange_authorization`)
//! - Account listing against Google Ads API v22 REST surface (`list_accounts`)
//! - Campaign CRUD: create, get, update, pause, resume, delete
//! - Campaign metrics reporting (`fetch_metrics`)
//! - Conversion upload (`upload`)

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::AdvertisingConfig;
use crate::domain::{
    AdPlatformPort, AdReviewObservation, AdReviewState, AdvertisingError, AuthorizationExchange,
    AuthorizationRequest, AuthorizationStart, Budget, BudgetType, Campaign, CampaignStatus,
    CapabilityMatrix, ConnectionAccessTokenProvider, ConnectionHandle, ConsentSnapshot,
    CreativeAssetSpec, CreativePlacement, DriftState, ExchangedAuthorization, GrantedAccessToken,
    HashedIds, MetricPoint, MetricQuery, Money, Objective, PlatformAccount, PlatformDescriptor,
    PlatformKey, RefreshTokenGrant, ScheduleGranularity, TargetingDimension, UploadedConversion,
};

const DEFAULT_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const DEFAULT_API_BASE: &str = "https://googleads.googleapis.com";
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

#[derive(Deserialize)]
struct GoogleMutateResponse {
    #[serde(default)]
    results: Vec<GoogleMutateResult>,
}

#[derive(Deserialize)]
struct GoogleMutateResult {
    #[serde(default, rename = "resourceName")]
    resource_name: String,
}

#[derive(Deserialize)]
struct GoogleSearchResponse {
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    results: Vec<GoogleSearchResultRow>,
}

#[derive(Deserialize)]
struct GoogleSearchResultRow {
    #[serde(default)]
    campaign: Option<GoogleCampaignRow>,
    #[serde(default)]
    segments: Option<GoogleSegmentsRow>,
    #[serde(default)]
    metrics: Option<GoogleMetricsRow>,
    #[serde(default)]
    customer: Option<GoogleCustomerRow>,
    #[serde(default, rename = "adGroupAd")]
    ad_group_ad: Option<GoogleAdGroupAdRow>,
}

#[derive(Deserialize)]
struct GoogleAdGroupAdRow {
    #[serde(default)]
    ad: Option<GoogleAdResourceRef>,
    #[serde(default, rename = "policySummary")]
    policy_summary: Option<GooglePolicySummaryRow>,
}

#[derive(Deserialize)]
struct GoogleAdResourceRef {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct GooglePolicySummaryRow {
    #[serde(default, rename = "approvalStatus")]
    approval_status: Option<String>,
    #[serde(default, rename = "policyTopicEntries")]
    policy_topic_entries: Vec<GooglePolicyTopicEntryRow>,
}

#[derive(Deserialize)]
struct GooglePolicyTopicEntryRow {
    #[serde(default)]
    topic: Option<String>,
}

#[derive(Deserialize)]
struct GoogleCampaignRow {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default, rename = "advertisingChannelType")]
    advertising_channel_type: Option<String>,
}

#[derive(Deserialize)]
struct GoogleSegmentsRow {
    #[serde(default)]
    date: Option<String>,
}

#[derive(Deserialize)]
struct GoogleMetricsRow {
    #[serde(default)]
    impressions: Option<serde_json::Value>,
    #[serde(default)]
    clicks: Option<serde_json::Value>,
    #[serde(default, rename = "costMicros")]
    cost_micros: Option<serde_json::Value>,
    #[serde(default)]
    conversions: Option<serde_json::Value>,
    #[serde(default, rename = "conversionsValue")]
    conversions_value: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct GoogleCustomerRow {
    #[serde(default, rename = "currencyCode")]
    currency_code: Option<String>,
}

/// The platform account id an account-scoped call addresses. Empty means the
/// caller handed us an authorization-time handle (see
/// `ConnectionHandle::external_account_id`); refuse rather than build a URL
/// around `customers//`, which Google answers with an opaque 404.
fn require_customer_id(connection: &ConnectionHandle) -> Result<String, AdvertisingError> {
    if connection.external_account_id.trim().is_empty() {
        return Err(AdvertisingError::InvalidInput(
            "connection has no external ad account id".to_string(),
        ));
    }
    Ok(format_customer_id(&connection.external_account_id))
}

/// The platform campaign id a GAQL query interpolates into a quoted literal.
/// Google campaign ids are bare numerals; anything else would break the query
/// syntax at best and — for a corrupted stored external id — smuggle extra
/// GAQL in at worst. Rejecting here hands the caller an `InvalidInput` naming
/// the id instead of an opaque platform error.
fn require_numeric_campaign_id(campaign_id: &str) -> Result<String, AdvertisingError> {
    let trimmed = campaign_id.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|character| character.is_ascii_digit()) {
        return Err(AdvertisingError::InvalidInput(format!(
            "google campaign id must be numeric, got {campaign_id:?}"
        )));
    }
    Ok(trimmed.to_string())
}

fn format_customer_id(raw_id: &str) -> String {
    let trimmed = raw_id
        .trim()
        .strip_prefix("customers/")
        .unwrap_or(raw_id.trim());
    trimmed.replace('-', "")
}

fn parse_json_i64(val: Option<&serde_json::Value>) -> i64 {
    match val {
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(serde_json::Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

fn parse_json_f64(val: Option<&serde_json::Value>) -> f64 {
    match val {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn map_status_to_google(status: CampaignStatus) -> &'static str {
    match status {
        CampaignStatus::Active => "ENABLED",
        CampaignStatus::Paused | CampaignStatus::Draft | CampaignStatus::Publishing => "PAUSED",
        CampaignStatus::Ended | CampaignStatus::Archived => "REMOVED",
    }
}

fn map_status_from_google(raw: &str) -> CampaignStatus {
    match raw {
        "ENABLED" => CampaignStatus::Active,
        "PAUSED" => CampaignStatus::Paused,
        "REMOVED" => CampaignStatus::Archived,
        _ => CampaignStatus::Paused,
    }
}

/// Map Google's `policy_summary.approval_status` onto the domain verdict.
/// The bool records whether policy topic entries carry decision-relevant
/// wording: they are the rejection reasons for a disapproval, and the
/// limiting policies for an approval-with-limits; plain approvals and
/// in-review states have none worth showing.
fn map_google_approval_status(raw: &str) -> (AdReviewState, bool) {
    match raw {
        "APPROVED" => (AdReviewState::Approved, false),
        "APPROVED_LIMITED" => (AdReviewState::Approved, true),
        "DISAPPROVED" => (AdReviewState::Rejected, true),
        // PENDING_REVIEW / UNDER_REVIEW / UNSPECIFIED / anything new: the
        // safe verdict is "still pending", never "approved".
        _ => (AdReviewState::Pending, false),
    }
}

fn map_channel_type_to_google(obj: &Objective) -> &'static str {
    match obj.as_str() {
        "sales" | "leads" | "traffic" => "SEARCH",
        "awareness" => "DISPLAY",
        "app_promotion" => "MULTI_CHANNEL",
        _ => "SEARCH",
    }
}

fn map_channel_type_from_google(raw: &str) -> Objective {
    let domain_key = match raw {
        "SEARCH" => "sales",
        "DISPLAY" => "awareness",
        "MULTI_CHANNEL" => "app_promotion",
        _ => "sales",
    };
    Objective::parse(domain_key).unwrap_or_else(|_| Objective::parse("sales").unwrap())
}

fn cost_micros_to_minor(cost_micros: i64, currency: &str) -> i64 {
    if currency == "JPY" || currency == "KRW" {
        cost_micros / 1_000_000
    } else {
        cost_micros / 10_000
    }
}

fn conversions_value_to_minor(val: f64, currency: &str) -> i64 {
    if currency == "JPY" || currency == "KRW" {
        val.round() as i64
    } else {
        (val * 100.0).round() as i64
    }
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
            client_secret: config.google_ads_client_secret.expose_secret().clone(),
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
        connection: &ConnectionHandle,
        campaign: Campaign,
        mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/campaigns:mutate",
            self.api_base_url, self.api_version
        );

        let status_str = map_status_to_google(campaign.status);
        let channel_type = map_channel_type_to_google(&campaign.objective);

        let payload = serde_json::json!({
            "operations": [
                {
                    "create": {
                        "name": campaign.name,
                        "status": status_str,
                        "advertisingChannelType": channel_type
                    }
                }
            ],
            "partialFailure": false,
            "validateOnly": false
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .header("x-mutation-id", mutation_id.to_string())
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google create_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google create_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google create_campaign failed with status {status}"
            )));
        }

        let body: GoogleMutateResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google create_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from google create_campaign".to_string(),
            )
        })?;

        let created_id = body
            .results
            .first()
            .and_then(|r| r.resource_name.rsplit('/').next().map(|id| id.to_string()))
            .unwrap_or_else(|| campaign.id.clone());

        let mut created = campaign;
        created.id = created_id;
        Ok(created)
    }

    async fn get_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Option<Campaign>, AdvertisingError> {
        let campaign_id = require_numeric_campaign_id(campaign_id)?;
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/googleAds:search",
            self.api_base_url, self.api_version
        );

        let gaql = format!(
            "SELECT campaign.id, campaign.name, campaign.status, campaign.advertising_channel_type FROM campaign WHERE campaign.id = '{campaign_id}'"
        );

        let payload = serde_json::json!({
            "query": gaql
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google get_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google get_campaign transport failed".to_string(),
                )
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google get_campaign failed with status {status}"
            )));
        }

        let body: GoogleSearchResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google get_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from google get_campaign".to_string(),
            )
        })?;

        if body.results.is_empty() {
            return Ok(None);
        }

        let row = &body.results[0];
        let c = match &row.campaign {
            Some(camp) => camp,
            None => return Ok(None),
        };

        let id = c.id.clone().unwrap_or_else(|| campaign_id.to_string());
        let name = c
            .name
            .clone()
            .unwrap_or_else(|| "Google Campaign".to_string());
        let status = c
            .status
            .as_deref()
            .map(map_status_from_google)
            .unwrap_or(CampaignStatus::Paused);
        let objective = c
            .advertising_channel_type
            .as_deref()
            .map(map_channel_type_from_google)
            .unwrap_or_else(|| Objective::parse("sales").unwrap());

        Ok(Some(Campaign {
            id,
            name,
            status,
            objective,
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(100, "JPY").unwrap(),
            },
            schedule: None,
            ad_groups: vec![],
            drift: DriftState {
                drifted: false,
                changed_fields: vec![],
            },
        }))
    }

    async fn update_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign: Campaign,
        mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/campaigns:mutate",
            self.api_base_url, self.api_version
        );

        let status_str = map_status_to_google(campaign.status);

        let payload = serde_json::json!({
            "operations": [
                {
                    "update": {
                        "resourceName": format!("customers/{customer_id}/campaigns/{}", campaign.id),
                        "name": campaign.name,
                        "status": status_str
                    },
                    "updateMask": "name,status"
                }
            ],
            "partialFailure": false,
            "validateOnly": false
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .header("x-mutation-id", mutation_id.to_string())
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google update_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google update_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google update_campaign failed with status {status}"
            )));
        }

        Ok(campaign)
    }

    async fn pause_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/campaigns:mutate",
            self.api_base_url, self.api_version
        );

        let payload = serde_json::json!({
            "operations": [
                {
                    "update": {
                        "resourceName": format!("customers/{customer_id}/campaigns/{campaign_id}"),
                        "status": "PAUSED"
                    },
                    "updateMask": "status"
                }
            ],
            "partialFailure": false,
            "validateOnly": false
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google pause_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google pause_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google pause_campaign failed with status {status}"
            )));
        }

        Ok(Campaign {
            id: campaign_id.to_string(),
            name: "Google Campaign".to_string(),
            status: CampaignStatus::Paused,
            objective: Objective::parse("sales").unwrap(),
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(100, "JPY").unwrap(),
            },
            schedule: None,
            ad_groups: vec![],
            drift: DriftState {
                drifted: false,
                changed_fields: vec![],
            },
        })
    }

    async fn resume_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Campaign, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/campaigns:mutate",
            self.api_base_url, self.api_version
        );

        let payload = serde_json::json!({
            "operations": [
                {
                    "update": {
                        "resourceName": format!("customers/{customer_id}/campaigns/{campaign_id}"),
                        "status": "ENABLED"
                    },
                    "updateMask": "status"
                }
            ],
            "partialFailure": false,
            "validateOnly": false
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google resume_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google resume_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google resume_campaign failed with status {status}"
            )));
        }

        Ok(Campaign {
            id: campaign_id.to_string(),
            name: "Google Campaign".to_string(),
            status: CampaignStatus::Active,
            objective: Objective::parse("sales").unwrap(),
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(100, "JPY").unwrap(),
            },
            schedule: None,
            ad_groups: vec![],
            drift: DriftState {
                drifted: false,
                changed_fields: vec![],
            },
        })
    }

    async fn delete_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<(), AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/campaigns:mutate",
            self.api_base_url, self.api_version
        );

        let payload = serde_json::json!({
            "operations": [
                {
                    "remove": format!("customers/{customer_id}/campaigns/{campaign_id}")
                }
            ],
            "partialFailure": false,
            "validateOnly": false
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google delete_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google delete_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google delete_campaign failed with status {status}"
            )));
        }

        Ok(())
    }

    async fn fetch_metrics(
        &self,
        connection: &ConnectionHandle,
        request: &MetricQuery,
    ) -> Result<Vec<MetricPoint>, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/googleAds:search",
            self.api_base_url, self.api_version
        );

        let gaql = format!(
            "SELECT campaign.id, segments.date, metrics.impressions, metrics.clicks, metrics.cost_micros, metrics.conversions, metrics.conversions_value, customer.currency_code FROM campaign WHERE segments.date BETWEEN '{}' AND '{}'",
            request.from, request.to
        );

        let payload = serde_json::json!({
            "query": gaql
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google fetch_metrics transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google fetch_metrics transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google fetch_metrics failed with status {status}"
            )));
        }

        let body: GoogleSearchResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google fetch_metrics JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from google fetch_metrics".to_string(),
            )
        })?;

        let mut points = Vec::new();
        for row in body.results {
            let campaign_id = row
                .campaign
                .as_ref()
                .and_then(|c| c.id.clone())
                .unwrap_or_default();
            let date = row
                .segments
                .as_ref()
                .and_then(|s| s.date.clone())
                .unwrap_or_default();
            let impressions =
                parse_json_i64(row.metrics.as_ref().and_then(|m| m.impressions.as_ref()));
            let clicks = parse_json_i64(row.metrics.as_ref().and_then(|m| m.clicks.as_ref()));
            let cost_micros =
                parse_json_i64(row.metrics.as_ref().and_then(|m| m.cost_micros.as_ref()));
            let conversions =
                parse_json_f64(row.metrics.as_ref().and_then(|m| m.conversions.as_ref()));
            let conversions_val = parse_json_f64(
                row.metrics
                    .as_ref()
                    .and_then(|m| m.conversions_value.as_ref()),
            );
            let currency = row
                .customer
                .as_ref()
                .and_then(|c| c.currency_code.clone())
                .unwrap_or_else(|| "JPY".to_string())
                .to_ascii_uppercase();

            let spend_minor = cost_micros_to_minor(cost_micros, &currency);
            let conv_val_minor = conversions_value_to_minor(conversions_val, &currency);

            points.push(MetricPoint {
                campaign_id,
                date,
                impressions,
                clicks,
                spend: Money::new(spend_minor, &currency)
                    .unwrap_or_else(|_| Money::new(0, "JPY").unwrap()),
                conversions,
                conversion_value: Money::new(conv_val_minor, &currency)
                    .unwrap_or_else(|_| Money::new(0, "JPY").unwrap()),
            });
        }

        Ok(points)
    }

    async fn fetch_review_status(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Vec<AdReviewObservation>, AdvertisingError> {
        let campaign_id = require_numeric_campaign_id(campaign_id)?;
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}/googleAds:search",
            self.api_base_url, self.api_version
        );

        // Policy topic entries are surfaced verbatim: the tenant must see
        // Google's own policy wording to fix the ad in Google's own tooling.
        let gaql = format!(
            "SELECT ad_group_ad.ad.id, ad_group_ad.status, ad_group_ad.policy_summary.approval_status, ad_group_ad.policy_summary.policy_topic_entries.topic FROM ad_group_ad WHERE campaign.id = '{campaign_id}' AND ad_group_ad.status != 'REMOVED'"
        );

        let mut observations = Vec::new();
        // `googleAds:search` pages its results; ignoring `nextPageToken`
        // would silently truncate the campaign's verdicts to the first page.
        let mut page_token = None;
        loop {
            let mut payload = serde_json::json!({ "query": gaql });
            if let Some(token) = &page_token {
                payload["pageToken"] = serde_json::json!(token);
            }

            let response = self
                .http
                .post(&url)
                .header("Authorization", format!("Bearer {access_token}"))
                .header("developer-token", &self.developer_token)
                .header(
                    "x-goog-request-params",
                    format!("customer_id={customer_id}"),
                )
                .json(&payload)
                .send()
                .await
                .map_err(|err| {
                    tracing::warn!(error = %err, "google fetch_review_status transport failed");
                    AdvertisingError::PlatformUnavailable(
                        "google fetch_review_status transport failed".to_string(),
                    )
                })?;

            if !response.status().is_success() {
                let status = response.status();
                return Err(AdvertisingError::PlatformUnavailable(format!(
                    "google fetch_review_status failed with status {status}"
                )));
            }

            let body: GoogleSearchResponse = response.json().await.map_err(|err| {
                tracing::warn!(error = %err, "google fetch_review_status JSON parse failed");
                AdvertisingError::PlatformUnavailable(
                    "invalid response shape from google fetch_review_status".to_string(),
                )
            })?;

            for row in body.results {
                let Some(ad_group_ad) = row.ad_group_ad else {
                    continue;
                };
                let Some(ad_external_id) = ad_group_ad.ad.and_then(|ad| ad.id) else {
                    continue;
                };
                let approval = ad_group_ad
                    .policy_summary
                    .as_ref()
                    .and_then(|summary| summary.approval_status.as_deref());
                let (state, attach_reasons) = match approval {
                    Some(status) => map_google_approval_status(status),
                    None => (AdReviewState::Pending, false),
                };
                let reasons = if attach_reasons {
                    ad_group_ad
                        .policy_summary
                        .map(|summary| {
                            summary
                                .policy_topic_entries
                                .into_iter()
                                .filter_map(|entry| entry.topic)
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                observations.push(AdReviewObservation {
                    ad_external_id,
                    state,
                    reasons,
                });
            }

            match body.next_page_token {
                Some(token) if !token.is_empty() => page_token = Some(token),
                _ => break,
            }
        }
        Ok(observations)
    }

    async fn upload(
        &self,
        connection: &ConnectionHandle,
        conversion: UploadedConversion,
        ids: HashedIds,
        consent: ConsentSnapshot,
    ) -> Result<(), AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!(
            "{}/{}/customers/{customer_id}:uploadClickConversions",
            self.api_base_url, self.api_version
        );

        let permitted = !consent
            .answers
            .values()
            .any(|ans| ans.eq_ignore_ascii_case("denied") || ans.eq_ignore_ascii_case("opt_out"));

        let mut user_identifiers = Vec::new();
        if permitted {
            for (key, val) in ids.values {
                let tag = match key.to_ascii_lowercase().as_str() {
                    "em" | "mail_hash" | "hashed_mail" => "hashedEmail".to_string(),
                    "ph" | "phone_hash" | "hashed_phone" => "hashedPhoneNumber".to_string(),
                    other => other.to_string(),
                };
                user_identifiers.push(serde_json::json!({
                    "userIdentifierSource": "FIRST_PARTY",
                    tag: val
                }));
            }
        }

        let payload = serde_json::json!({
            "conversions": [
                {
                    "conversionAction": format!("customers/{customer_id}/conversionActions/default"),
                    "conversionDateTime": Utc::now().format("%Y-%m-%d %H:%M:%S%:z").to_string(),
                    "orderId": conversion.event_id,
                    "userIdentifiers": user_identifiers
                }
            ],
            "partialFailure": true
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google upload conversions transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google upload conversions transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google upload conversions failed with status {status}"
            )));
        }

        Ok(())
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

/// Real Google Ads conversion sink: `uploadClickConversions` for uploads and
/// `uploadConversionAdjustments` (RETRACTION) for post-upload withdrawals.
/// Identifiers arrive already hashed — the sink never sees raw PII — and the
/// consent snapshot rides on the event, so a consented and a suppressed
/// upload cannot be confused at the transport layer.
pub struct GoogleConversionSink {
    http: reqwest::Client,
    developer_token: String,
    api_version: String,
    api_base_url: String,
    access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    descriptor: PlatformDescriptor,
}

impl GoogleConversionSink {
    pub fn new(
        config: &AdvertisingConfig,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    ) -> Result<Self, AdvertisingError> {
        Self::with_custom_endpoints(config, DEFAULT_API_BASE.to_string(), access_tokens)
    }

    pub fn with_custom_endpoints(
        config: &AdvertisingConfig,
        api_base_url: String,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    ) -> Result<Self, AdvertisingError> {
        Self::with_custom_endpoints_and_timeout(
            config,
            api_base_url,
            access_tokens,
            REQUEST_TIMEOUT,
        )
    }

    /// Test constructor: a shortened request deadline so a slow Google Ads
    /// is observable in milliseconds. The connect phase keeps the matrix's
    /// 5 s budget; the error contract (transport failure →
    /// `PlatformUnavailable`) is identical.
    pub fn with_custom_endpoints_and_timeout(
        config: &AdvertisingConfig,
        api_base_url: String,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
        request_timeout: std::time::Duration,
    ) -> Result<Self, AdvertisingError> {
        if config.google_ads_developer_token.trim().is_empty() {
            return Err(AdvertisingError::PlatformUnavailable(
                "google ads developer token is required for the conversion sink".to_string(),
            ));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(request_timeout)
                .build()
                .map_err(|err| {
                    AdvertisingError::PlatformUnavailable(format!(
                        "google conversion sink http client: {err}"
                    ))
                })?,
            developer_token: config.google_ads_developer_token.expose_secret().clone(),
            api_version: config.google_ads_api_version.clone(),
            api_base_url,
            access_tokens,
            descriptor: google_ads_descriptor(),
        })
    }

    /// POST a version-rooted path; callers build the customer-rooted suffix
    /// themselves because RPC calls (`:verb`) and job resources nest
    /// differently.
    async fn post(
        &self,
        connection: &ConnectionHandle,
        path: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!("{}/{}/{path}", self.api_base_url, self.api_version);
        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google conversion sink transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google conversion sink transport failed".to_string(),
                )
            })?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google conversion sink {path} failed with status {status}"
            )));
        }
        response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google conversion sink JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from google conversion sink".to_string(),
            )
        })
    }
}

/// REST field names for `UserIdentifier`: Google's REST interface uses
/// lowerCamelCase proto JSON (`hashedEmail`, `hashedPhoneNumber`), not the
/// snake_case proto field names — a `hashed_mail` key is silently ignored and
/// the identifier never matches, gutting match rate while the upload still
/// answers 200.
fn google_user_identifiers(
    hashed: &std::collections::BTreeMap<String, crate::domain::HashedIdentifier>,
) -> Vec<serde_json::Value> {
    hashed
        .iter()
        .map(|(key, hash)| {
            let lowered = key.to_ascii_lowercase();
            let tag = match lowered.as_str() {
                "em" | "mail_hash" | "hashed_mail" => "hashedEmail",
                "ph" | "phone_hash" | "hashed_phone" => "hashedPhoneNumber",
                other => other,
            };
            serde_json::json!({
                "userIdentifierSource": "FIRST_PARTY",
                tag: hash.as_str()
            })
        })
        .collect()
}

#[async_trait]
impl crate::domain::ConversionSinkPort for GoogleConversionSink {
    fn descriptor(&self) -> PlatformDescriptor {
        self.descriptor.clone()
    }

    async fn upload(
        &self,
        connection: &ConnectionHandle,
        event: &crate::domain::ConversionEvent,
    ) -> Result<crate::domain::UploadReceipt, AdvertisingError> {
        let permitted =
            !event.consent.answers.values().any(|ans| {
                ans.eq_ignore_ascii_case("denied") || ans.eq_ignore_ascii_case("opt_out")
            });

        let mut conversion = serde_json::json!({
            // The conversion action is provisioned per tenant account during
            // connection setup; until that registry ships, the well-known
            // default action is the documented interim (tracked in the
            // TASK-012 row alongside the live-credential gap).
            "conversionAction": format!(
                "customers/{}/conversionActions/default",
                require_customer_id(connection)?
            ),
            "conversionDateTime": event
                .occurred_at
                .format("%Y-%m-%d %H:%M:%S%:z")
                .to_string(),
            "orderId": event
                .order_ref
                .clone()
                .unwrap_or_else(|| event.event_id.clone()),
        });
        if let Some(gclid) = &event.click_ids.gclid {
            conversion["gclid"] = serde_json::Value::String(gclid.clone());
        }
        if let Some(gbraid) = &event.click_ids.gbraid {
            conversion["gbraid"] = serde_json::Value::String(gbraid.clone());
        }
        if let Some(wbraid) = &event.click_ids.wbraid {
            conversion["wbraid"] = serde_json::Value::String(wbraid.clone());
        }
        if let Some(value) = &event.value {
            // `conversionValue` is a major-unit double; stored money is minor
            // units, so the currency's exponent divides it back out.
            // `currencyCode` names the value's currency — without it Google
            // reads the amount in the account currency, which silently
            // corrupts value-based bidding for every other currency.
            let major =
                value.amount_minor as f64 / 10_f64.powi(Money::exponent(&value.currency) as i32);
            conversion["conversionValue"] = serde_json::json!(major);
            conversion["currencyCode"] = serde_json::json!(value.currency);
        }
        if permitted {
            let identifiers = google_user_identifiers(&event.hashed_identifiers);
            if !identifiers.is_empty() {
                conversion["userIdentifiers"] = serde_json::Value::Array(identifiers);
            }
        }

        let customer_id = require_customer_id(connection)?;
        self.post(
            connection,
            &format!("customers/{customer_id}:uploadClickConversions"),
            serde_json::json!({ "conversions": [conversion], "partialFailure": true }),
        )
        .await?;

        Ok(crate::domain::UploadReceipt {
            platform: self.descriptor.key.clone(),
            external_id: event.event_id.clone(),
            uploaded_at: Utc::now(),
        })
    }

    async fn retract(
        &self,
        connection: &ConnectionHandle,
        event: &crate::domain::ConversionEvent,
    ) -> Result<(), AdvertisingError> {
        let adjustment = serde_json::json!({
            "adjustmentType": "RETRACTION",
            "conversionAction": format!(
                "customers/{}/conversionActions/default",
                require_customer_id(connection)?
            ),
            "conversionDateTime": event
                .occurred_at
                .format("%Y-%m-%d %H:%M:%S%:z")
                .to_string(),
            "orderId": event
                .order_ref
                .clone()
                .unwrap_or_else(|| event.event_id.clone()),
        });
        let customer_id = require_customer_id(connection)?;
        self.post(
            connection,
            &format!("customers/{customer_id}:uploadConversionAdjustments"),
            serde_json::json!({ "conversionAdjustments": [adjustment], "partialFailure": true }),
        )
        .await?;
        Ok(())
    }
}

/// Real Google Ads audience sync through the offline-user-data-job surface.
/// All members ride `hashedEmail` — the port's members are type-less hashes
/// today, so a subject carrying only a telephone-number hash would be
/// mislabelled; the port must grow
/// typed members before the live flag flips (recorded in the TASK-015 row).
pub struct GoogleAudienceSync {
    http: reqwest::Client,
    developer_token: String,
    api_version: String,
    api_base_url: String,
    access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    descriptor: PlatformDescriptor,
}

impl GoogleAudienceSync {
    pub fn with_custom_endpoints(
        config: &AdvertisingConfig,
        api_base_url: String,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    ) -> Result<Self, AdvertisingError> {
        if config.google_ads_developer_token.trim().is_empty() {
            return Err(AdvertisingError::PlatformUnavailable(
                "google ads developer token is required for the audience sink".to_string(),
            ));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .map_err(|err| {
                    AdvertisingError::PlatformUnavailable(format!(
                        "google audience sink http client: {err}"
                    ))
                })?,
            developer_token: config.google_ads_developer_token.expose_secret().clone(),
            api_version: config.google_ads_api_version.clone(),
            api_base_url,
            access_tokens,
            descriptor: google_ads_descriptor(),
        })
    }

    async fn post(
        &self,
        connection: &ConnectionHandle,
        path: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let customer_id = require_customer_id(connection)?;
        let url = format!("{}/{}/{path}", self.api_base_url, self.api_version);
        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("developer-token", &self.developer_token)
            .header(
                "x-goog-request-params",
                format!("customer_id={customer_id}"),
            )
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "google audience sink transport failed");
                AdvertisingError::PlatformUnavailable(
                    "google audience sink transport failed".to_string(),
                )
            })?;
        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "google audience sink {path} failed with status {status}"
            )));
        }
        response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "google audience sink JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from google audience sink".to_string(),
            )
        })
    }
}

#[async_trait]
impl crate::domain::AudienceSyncPort for GoogleAudienceSync {
    fn descriptor(&self) -> PlatformDescriptor {
        self.descriptor.clone()
    }

    async fn sync_audience(
        &self,
        connection: &ConnectionHandle,
        external_ref: Option<&str>,
        included: &[crate::domain::HashedIdentifier],
        removed: &[crate::domain::HashedIdentifier],
    ) -> Result<crate::domain::AudienceSyncReceipt, AdvertisingError> {
        let Some(list_name) = external_ref else {
            return Err(AdvertisingError::InvalidInput(
                "google audience sync requires the platform user list resource name".to_string(),
            ));
        };
        if included.is_empty() && removed.is_empty() {
            return Ok(crate::domain::AudienceSyncReceipt {
                external_ref: list_name.to_string(),
            });
        }

        // One offline-user-data-job per sync: create → add operations → run.
        // Removals ride the same job as additions so a refresh applies as one
        // unit, and an opted-out member cannot survive a race between the add
        // batch and the remove batch.
        let customer_id = require_customer_id(connection)?;
        let job = self
            .post(
                connection,
                &format!("customers/{customer_id}/offlineUserDataJobs:create"),
                serde_json::json!({
                    "type": "USER_LIST",
                    "userList": list_name,
                }),
            )
            .await?;
        let job_name = job
            .get("resourceName")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                AdvertisingError::PlatformUnavailable(
                    "google audience sink job create returned no resourceName".to_string(),
                )
            })?
            .to_string();

        let mut operations = Vec::new();
        for member in included {
            operations.push(serde_json::json!({
                "create": { "userIdentifiers": [ { "hashedEmail": member.as_str() } ] }
            }));
        }
        for member in removed {
            operations.push(serde_json::json!({
                "remove": { "userIdentifiers": [ { "hashedEmail": member.as_str() } ] }
            }));
        }
        self.post(
            connection,
            &format!("{job_name}:addOperations"),
            serde_json::json!({ "operations": operations, "enablePartialFailure": true }),
        )
        .await?;
        self.post(
            connection,
            &format!("{job_name}:run"),
            serde_json::json!({}),
        )
        .await?;

        Ok(crate::domain::AudienceSyncReceipt {
            external_ref: list_name.to_string(),
        })
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
            client_secret: config.google_ads_client_secret.expose_secret().clone(),
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
    use sanvi_telemetry::Redacted;

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
                            let mut buf = vec![0u8; 8192];
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
            google_ads_developer_token: Redacted::new("test_dev_token_123".to_string()),
            google_ads_client_id: "test_client_id_456".to_string(),
            google_ads_client_secret: Redacted::new("test_client_secret_789".to_string()),
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
            google_ads_developer_token: Redacted::new("   ".to_string()),
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
            external_account_id: "conn-123".to_string(),
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

        let connection = ConnectionHandle {
            id: "7318492050313216".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "7318492050313216".to_string(),
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
    async fn fetch_review_status_maps_policy_topics_verbatim() {
        let review_fixture = serde_json::json!({
            "results": [
                {
                    "adGroupAd": {
                        "ad": { "id": "111" },
                        "status": "ENABLED",
                        "policySummary": {
                            "approvalStatus": "DISAPPROVED",
                            "policyTopicEntries": [
                                { "topic": "MISREPRESENTATION" },
                                { "topic": "SHOCKING_CONTENT" }
                            ]
                        }
                    }
                },
                {
                    "adGroupAd": {
                        "ad": { "id": "222" },
                        "status": "ENABLED",
                        "policySummary": { "approvalStatus": "APPROVED" }
                    }
                },
                {
                    "adGroupAd": {
                        "ad": { "id": "333" },
                        "status": "ENABLED",
                        "policySummary": { "approvalStatus": "PENDING_REVIEW" }
                    }
                },
                {
                    "adGroupAd": {
                        "ad": { "id": "444" },
                        "status": "ENABLED",
                        "policySummary": {
                            "approvalStatus": "APPROVED_LIMITED",
                            "policyTopicEntries": [{ "topic": "MISLEADING" }]
                        }
                    }
                }
            ]
        });

        let mock = MockServer::start(vec![(200, review_fixture)]).await;

        let adapter = GoogleAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}/token", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            id: "7318492050313216".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "7318492050313216".to_string(),
        };

        let observations = adapter
            .fetch_review_status(&connection, "120330001")
            .await
            .expect("review status fetch succeeds");

        assert_eq!(observations.len(), 4);
        assert_eq!(observations[0].ad_external_id, "111");
        assert_eq!(observations[0].state, AdReviewState::Rejected);
        assert_eq!(
            observations[0].reasons,
            vec![
                "MISREPRESENTATION".to_string(),
                "SHOCKING_CONTENT".to_string()
            ]
        );
        assert_eq!(observations[1].state, AdReviewState::Approved);
        assert!(observations[1].reasons.is_empty());
        assert_eq!(observations[2].state, AdReviewState::Pending);
        assert_eq!(observations[3].state, AdReviewState::Approved);
        assert_eq!(observations[3].reasons, vec!["MISLEADING".to_string()]);

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].path,
            "/v22/customers/7318492050313216/googleAds:search"
        );
        let body = String::from_utf8_lossy(&recorded[0].body);
        assert!(
            body.contains("FROM ad_group_ad"),
            "review query targets ad_group_ad: {body}"
        );
        assert!(body.contains("campaign.id = '120330001'"));
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
    async fn account_scoped_calls_address_the_external_account_not_the_connection_id() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({
                "results": [ { "resourceName": "customers/1234567890/campaigns/555666777" } ]
            }),
        )])
        .await;
        let adapter = GoogleAdsAdapter::with_custom_endpoints(
            &test_config(),
            "https://accounts.example/o/oauth2/v2/auth".to_string(),
            "https://oauth2.example/token".to_string(),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            id: "8331002299445566".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "123-456-7890".to_string(),
        };

        adapter
            .pause_campaign(&connection, "555666777")
            .await
            .expect("pause campaign succeeds");

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].path.contains("/customers/1234567890/"),
            "expected the dash-stripped external account id in {}",
            recorded[0].path
        );
        assert!(
            !recorded[0].path.contains("8331002299445566"),
            "the Sanvi connection id must never reach the platform: {}",
            recorded[0].path
        );
    }

    #[tokio::test]
    async fn account_scoped_calls_refuse_a_handle_with_no_external_account() {
        let adapter = GoogleAdsAdapter::new(&test_config(), stub_tokens("ya29.unused")).unwrap();
        let connection = ConnectionHandle {
            id: "8331002299445566".to_string(),
            tenant_id: test_tenant(),
            external_account_id: String::new(),
        };

        // No mock server is running: reaching the network at all would fail
        // differently, so `InvalidInput` proves the guard rejected first.
        assert!(matches!(
            adapter.pause_campaign(&connection, "555666777").await,
            Err(AdvertisingError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn google_campaign_crud_and_metrics_operations() {
        let mutate_create_resp = serde_json::json!({
            "results": [
                { "resourceName": "customers/1234567890/campaigns/555666777" }
            ]
        });
        let search_get_resp = serde_json::json!({
            "results": [
                {
                    "campaign": {
                        "id": "555666777",
                        "name": "Google Search Q3",
                        "status": "ENABLED",
                        "advertisingChannelType": "SEARCH"
                    }
                }
            ]
        });
        let mutate_update_resp = serde_json::json!({
            "results": [
                { "resourceName": "customers/1234567890/campaigns/555666777" }
            ]
        });
        let mutate_pause_resp = serde_json::json!({
            "results": [
                { "resourceName": "customers/1234567890/campaigns/555666777" }
            ]
        });
        let mutate_resume_resp = serde_json::json!({
            "results": [
                { "resourceName": "customers/1234567890/campaigns/555666777" }
            ]
        });
        let mutate_delete_resp = serde_json::json!({
            "results": [
                { "resourceName": "customers/1234567890/campaigns/555666777" }
            ]
        });
        let search_metrics_resp = serde_json::json!({
            "results": [
                {
                    "campaign": { "id": "555666777" },
                    "segments": { "date": "2026-08-05" },
                    "metrics": {
                        "impressions": "2400",
                        "clicks": "120",
                        "costMicros": "45000000",
                        "conversions": 8.5,
                        "conversionsValue": 150.0
                    },
                    "customer": { "currencyCode": "USD" }
                }
            ]
        });
        let upload_resp = serde_json::json!({
            "results": [ { "orderId": "evt-order-1" } ]
        });

        let mock = MockServer::start(vec![
            (200, mutate_create_resp),
            (200, search_get_resp),
            (200, mutate_update_resp),
            (200, mutate_pause_resp),
            (200, mutate_resume_resp),
            (200, mutate_delete_resp),
            (200, search_metrics_resp),
            (200, upload_resp),
        ])
        .await;

        let adapter = GoogleAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}/token", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            // Deliberately unlike the account id: the platform only ever hears
            // about `external_account_id`, never Sanvi's own connection id.
            id: "8331002299445566".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "1234567890".to_string(),
        };

        let campaign = Campaign {
            id: "temp-draft-id".to_string(),
            name: "Google Search Q3".to_string(),
            status: CampaignStatus::Active,
            objective: Objective::parse("sales").unwrap(),
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(1000, "USD").unwrap(),
            },
            schedule: None,
            ad_groups: vec![],
            drift: DriftState {
                drifted: false,
                changed_fields: vec![],
            },
        };

        // 1. Create
        let created = adapter
            .create_campaign(&connection, campaign.clone(), Uuid::now_v7())
            .await
            .expect("create campaign succeeds");
        assert_eq!(created.id, "555666777");

        // 2. Get
        let fetched = adapter
            .get_campaign(&connection, "555666777")
            .await
            .expect("get campaign succeeds")
            .expect("campaign found");
        assert_eq!(fetched.id, "555666777");
        assert_eq!(fetched.name, "Google Search Q3");
        assert_eq!(fetched.status, CampaignStatus::Active);

        // 3. Update
        let updated = adapter
            .update_campaign(&connection, fetched, Uuid::now_v7())
            .await
            .expect("update campaign succeeds");
        assert_eq!(updated.id, "555666777");

        // 4. Pause
        let paused = adapter
            .pause_campaign(&connection, "555666777")
            .await
            .expect("pause campaign succeeds");
        assert_eq!(paused.status, CampaignStatus::Paused);

        // 5. Resume
        let resumed = adapter
            .resume_campaign(&connection, "555666777")
            .await
            .expect("resume campaign succeeds");
        assert_eq!(resumed.status, CampaignStatus::Active);

        // 6. Delete
        adapter
            .delete_campaign(&connection, "555666777")
            .await
            .expect("delete campaign succeeds");

        // 7. Fetch Metrics
        let metrics = adapter
            .fetch_metrics(
                &connection,
                &MetricQuery {
                    from: "2026-08-05".to_string(),
                    to: "2026-08-05".to_string(),
                },
            )
            .await
            .expect("fetch metrics succeeds");
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].impressions, 2400);
        assert_eq!(metrics[0].clicks, 120);
        assert_eq!(metrics[0].spend.amount_minor, 4500); // 45,000,000 micros = $45.00 = 4500 cents
        assert_eq!(metrics[0].spend.currency, "USD");
        assert_eq!(metrics[0].conversions, 8.5);
        assert_eq!(metrics[0].conversion_value.amount_minor, 15000); // 150.0 USD = 15000 cents

        // 8. Upload
        adapter
            .upload(
                &connection,
                UploadedConversion {
                    event_id: "evt-order-1".to_string(),
                    event_name: "purchase".to_string(),
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
            .expect("upload succeeds");
    }

    #[tokio::test]
    async fn get_campaign_returns_none_when_empty_search() {
        let empty_search_resp = serde_json::json!({
            "results": []
        });
        let mock = MockServer::start(vec![(200, empty_search_resp)]).await;

        let adapter = GoogleAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}/token", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            id: "1234567890".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "1234567890".to_string(),
        };

        let result = adapter
            .get_campaign(&connection, "9999999999")
            .await
            .expect("get_campaign query succeeds");
        assert!(result.is_none());
    }

    use crate::domain::{AudienceSyncPort, ConsentSnapshot, ConversionEvent, ConversionSinkPort};

    fn sink_event() -> ConversionEvent {
        ConversionEvent {
            id: crate::domain::ConversionId::try_from(1_000_900_i64).unwrap(),
            tenant_id: test_tenant(),
            event_id: "evt-sink-1".to_string(),
            dedupe_key: "test:evt-sink-1".to_string(),
            name: "purchase".to_string(),
            occurred_at: Utc::now(),
            value: Some(Money::new(1_000, "USD").unwrap()),
            value_source: Some(crate::domain::ValueSource::PaymentRecord),
            order_ref: Some("order-sink-1".to_string()),
            click_ids: crate::domain::ClickIds {
                gclid: Some("test-gclid".to_string()),
                ..Default::default()
            },
            hashed_identifiers: std::collections::BTreeMap::from([(
                "em".to_string(),
                crate::domain::hashing::hash_email("buyer@example.com"),
            )]),
            consent: ConsentSnapshot {
                resolver_version: "v1".to_string(),
                jurisdiction: "US-CA".to_string(),
                purposes_asked: vec!["ads_measurement".to_string()],
                answers: std::collections::BTreeMap::new(),
                signal_source: "default".to_string(),
            },
            upload_states: std::collections::BTreeMap::new(),
            subject_key: crate::domain::SubjectKeyRef::TenantDevice {
                device_ref: "device-sink".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn conversion_sink_uploads_click_conversion_and_retracts() {
        let mock = MockServer::start(vec![
            (200, serde_json::json!({})),
            (200, serde_json::json!({})),
        ])
        .await;
        let sink = GoogleConversionSink::with_custom_endpoints(
            &test_config(),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();
        let connection = ConnectionHandle {
            id: "conn-sink".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "7318492050313216".to_string(),
        };
        let event = sink_event();

        let receipt = sink
            .upload(&connection, &event)
            .await
            .expect("google sink upload succeeds");
        assert_eq!(receipt.external_id, "evt-sink-1");
        assert_eq!(receipt.platform.as_str(), "google_ads");

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].path,
            "/v22/customers/7318492050313216:uploadClickConversions"
        );
        let body: serde_json::Value = serde_json::from_slice(&recorded[0].body).unwrap();
        let conversion = &body["conversions"][0];
        assert_eq!(conversion["orderId"], "order-sink-1");
        assert_eq!(conversion["gclid"], "test-gclid");
        assert!(
            conversion["userIdentifiers"][0]["hashedEmail"]
                .as_str()
                .is_some(),
            "hashed identifiers ride lowerCamelCase, the REST field names Google matches on: {conversion}"
        );
        // 1000 minor units USD → 10.0 major units, labelled with its currency.
        assert_eq!(conversion["conversionValue"], 10.0);
        assert_eq!(conversion["currencyCode"], "USD");

        sink.retract(&connection, &event)
            .await
            .expect("google sink retraction succeeds");
        let recorded = mock.recorded();
        assert_eq!(
            recorded[1].path,
            "/v22/customers/7318492050313216:uploadConversionAdjustments"
        );
        let body: serde_json::Value = serde_json::from_slice(&recorded[1].body).unwrap();
        assert_eq!(
            body["conversionAdjustments"][0]["adjustmentType"],
            "RETRACTION"
        );
        assert_eq!(body["conversionAdjustments"][0]["orderId"], "order-sink-1");
    }

    #[tokio::test]
    async fn audience_sink_runs_one_offline_user_data_job() {
        let job_name = "customers/7318492050313216/offlineUserDataJobs/1";
        let mock = MockServer::start(vec![
            (200, serde_json::json!({ "resourceName": job_name })),
            (200, serde_json::json!({})),
            (200, serde_json::json!({})),
        ])
        .await;
        let sink = GoogleAudienceSync::with_custom_endpoints(
            &test_config(),
            format!("http://{}", mock.addr),
            stub_tokens("ya29.provider_issued_access_token"),
        )
        .unwrap();
        let connection = ConnectionHandle {
            id: "conn-aud".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "7318492050313216".to_string(),
        };

        let included = vec![crate::domain::hashing::hash_email("keep@example.com")];
        let removed = vec![crate::domain::hashing::hash_email("drop@example.com")];
        let receipt = sink
            .sync_audience(
                &connection,
                Some("customers/7318492050313216/userLists/9"),
                &included,
                &removed,
            )
            .await
            .expect("google audience sync succeeds");
        assert_eq!(
            receipt.external_ref,
            "customers/7318492050313216/userLists/9"
        );

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 3, "create → addOperations → run");
        assert!(
            recorded[0].path.ends_with("/offlineUserDataJobs:create"),
            "{}",
            recorded[0].path
        );
        assert_eq!(recorded[1].path, format!("/v22/{job_name}:addOperations"));
        let body: serde_json::Value = serde_json::from_slice(&recorded[1].body).unwrap();
        assert_eq!(body["operations"].as_array().unwrap().len(), 2);
        assert_eq!(
            body["operations"][0]["create"]["userIdentifiers"][0]["hashedEmail"],
            included[0].as_str()
        );
        assert_eq!(
            body["operations"][1]["remove"]["userIdentifiers"][0]["hashedEmail"],
            removed[0].as_str()
        );
        assert_eq!(recorded[2].path, format!("/v22/{job_name}:run"));
    }

    #[tokio::test]
    async fn audience_sync_requires_the_platform_list_ref() {
        let sink = GoogleAudienceSync::with_custom_endpoints(
            &test_config(),
            "http://127.0.0.1:9".to_string(),
            stub_tokens("ya29.unused"),
        )
        .unwrap();
        let connection = ConnectionHandle {
            id: "conn-aud".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "7318492050313216".to_string(),
        };
        let err = sink
            .sync_audience(&connection, None, &[], &[])
            .await
            .expect_err("missing list ref is refused");
        assert!(matches!(err, AdvertisingError::InvalidInput(_)));
    }
}
