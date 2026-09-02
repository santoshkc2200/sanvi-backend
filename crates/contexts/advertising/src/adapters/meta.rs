//! Meta Marketing API REST adapter (Graph API, slice 10.4).
//!
//! Covers:
//! - OAuth 2.0 authorization URL construction (`begin_authorization`)
//! - OAuth authorization-code exchange + long-lived token exchange (`exchange_authorization`)
//! - Account listing against Graph API (`list_accounts`)
//! - Campaign CRUD: create, get, update, pause, resume, delete
//! - Campaign insights / metrics reporting (`fetch_metrics`)
//! - Conversions API event upload (`upload`)
//! - Refresh token grant (`MetaRefreshTokenGrant`)

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
    AuthorizationStart, Budget, BudgetType, Campaign, CampaignStatus, CapabilityMatrix,
    ConnectionAccessTokenProvider, ConnectionHandle, ConsentSnapshot, CreativeAssetSpec,
    CreativePlacement, DriftState, ExchangedAuthorization, GrantedAccessToken, HashedIds,
    MetricPoint, MetricQuery, Money, Objective, PlatformAccount, PlatformDescriptor, PlatformKey,
    RefreshTokenGrant, ScheduleGranularity, TargetingDimension, UploadedConversion,
};

const META_SCOPE_ADS_MANAGEMENT: &str = "ads_management";
const META_SCOPE_ADS_READ: &str = "ads_read";
const META_SCOPE_BUSINESS_MANAGEMENT: &str = "business_management";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Meta Ads REST adapter for Graph API.
pub struct MetaAdsAdapter {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    api_version: String,
    auth_base_url: String,
    graph_base_url: String,
    access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    descriptor: PlatformDescriptor,
}

impl std::fmt::Debug for MetaAdsAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaAdsAdapter")
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .field("api_version", &self.api_version)
            .field("auth_base_url", &self.auth_base_url)
            .field("graph_base_url", &self.graph_base_url)
            .finish()
    }
}

#[derive(Serialize)]
struct MetaAuthQueryParams<'a> {
    client_id: &'a str,
    redirect_uri: &'a str,
    response_type: &'a str,
    scope: &'a str,
    state: &'a str,
}

#[derive(Deserialize)]
struct MetaTokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    error: Option<MetaApiErrorDetail>,
}

#[derive(Deserialize, Clone, Debug)]
struct MetaApiErrorDetail {
    #[serde(default)]
    message: String,
    #[serde(default)]
    code: i64,
}

#[derive(Deserialize)]
struct MetaErrorEnvelope {
    error: MetaApiErrorDetail,
}

#[derive(Deserialize)]
struct MetaAdAccountNode {
    id: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct MetaAdAccountsResponse {
    #[serde(default)]
    data: Vec<MetaAdAccountNode>,
    #[serde(default)]
    error: Option<MetaApiErrorDetail>,
}

#[derive(Deserialize)]
struct MetaIdResponse {
    id: String,
}

#[derive(Deserialize)]
struct MetaSuccessResponse {
    #[serde(default)]
    error: Option<MetaApiErrorDetail>,
}

#[derive(Deserialize)]
struct MetaCampaignNode {
    id: String,
    name: String,
    objective: Option<String>,
    status: Option<String>,
    daily_budget: Option<String>,
    lifetime_budget: Option<String>,
}

#[derive(Deserialize)]
struct MetaInsightAction {
    value: String,
}

#[derive(Deserialize)]
struct MetaInsightRow {
    campaign_id: String,
    date_start: String,
    #[serde(default)]
    impressions: Option<String>,
    #[serde(default)]
    clicks: Option<String>,
    #[serde(default)]
    spend: Option<String>,
    #[serde(default)]
    actions: Option<Vec<MetaInsightAction>>,
    #[serde(default)]
    action_values: Option<Vec<MetaInsightAction>>,
    #[serde(default)]
    account_currency: Option<String>,
}

#[derive(Deserialize)]
struct MetaInsightsResponse {
    #[serde(default)]
    data: Vec<MetaInsightRow>,
    #[serde(default)]
    error: Option<MetaApiErrorDetail>,
}

fn format_act_id(raw_id: &str) -> String {
    let trimmed = raw_id.trim();
    if trimmed.starts_with("act_") {
        trimmed.to_string()
    } else {
        format!("act_{trimmed}")
    }
}

/// The Meta ad account this call addresses, in `act_<id>` form. Empty means an
/// authorization-time handle (see `ConnectionHandle::external_account_id`);
/// refuse rather than address `act_`, which Graph answers with an opaque error.
fn require_act_id(connection: &ConnectionHandle) -> Result<String, AdvertisingError> {
    if connection.external_account_id.trim().is_empty() {
        return Err(AdvertisingError::InvalidInput(
            "connection has no external ad account id".to_string(),
        ));
    }
    Ok(format_act_id(&connection.external_account_id))
}

fn strip_act_prefix(raw_id: &str) -> &str {
    raw_id.trim().strip_prefix("act_").unwrap_or(raw_id.trim())
}

impl MetaAdsAdapter {
    pub fn new(
        config: &AdvertisingConfig,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    ) -> Result<Self, AdvertisingError> {
        let api_version = if config.meta_api_version.trim().is_empty() {
            "v24.0".to_string()
        } else {
            config.meta_api_version.trim().to_string()
        };
        let auth_base_url = format!("https://www.facebook.com/{api_version}/dialog/oauth");
        let graph_base_url = format!("https://graph.facebook.com/{api_version}");
        Self::with_custom_endpoints(config, auth_base_url, graph_base_url, access_tokens)
    }

    pub fn with_custom_endpoints(
        config: &AdvertisingConfig,
        auth_base_url: String,
        graph_base_url: String,
        access_tokens: Arc<dyn ConnectionAccessTokenProvider>,
    ) -> Result<Self, AdvertisingError> {
        let client_id = config.meta_client_id.trim().to_string();
        if client_id.is_empty() {
            return Err(AdvertisingError::PlatformUnavailable(
                "meta client id is required".to_string(),
            ));
        }

        let api_version = if config.meta_api_version.trim().is_empty() {
            "v24.0".to_string()
        } else {
            config.meta_api_version.trim().to_string()
        };

        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| AdvertisingError::PlatformUnavailable(err.to_string()))?;

        Ok(Self {
            http,
            client_id,
            client_secret: config.meta_client_secret.clone(),
            api_version,
            auth_base_url,
            graph_base_url,
            access_tokens,
            descriptor: meta_descriptor(),
        })
    }

    async fn fetch_ad_accounts(
        &self,
        access_token: &str,
    ) -> Result<Vec<PlatformAccount>, AdvertisingError> {
        let url = format!("{}/me/adaccounts", self.graph_base_url);
        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .query(&[("fields", "id,name,account_status")])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta me/adaccounts transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta me/adaccounts transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!(status = %status, "meta me/adaccounts returned non-success");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta ad account listing failed with status {status}"
            )));
        }

        let body: MetaAdAccountsResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta me/adaccounts JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta ad account listing".to_string(),
            )
        })?;

        if let Some(err) = body.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported error: {}",
                err.message
            )));
        }

        let accounts = body
            .data
            .into_iter()
            .map(|node| {
                let external_id = strip_act_prefix(&node.id).to_string();
                let display_name = node
                    .name
                    .unwrap_or_else(|| format!("Meta Ad Account ({external_id})"));
                PlatformAccount {
                    external_id,
                    display_name,
                }
            })
            .collect();

        Ok(accounts)
    }

    fn map_objective_to_meta(obj: &Objective) -> &'static str {
        match obj.as_str() {
            "sales" => "OUTCOME_SALES",
            "leads" => "OUTCOME_LEADS",
            "traffic" => "OUTCOME_TRAFFIC",
            "awareness" => "OUTCOME_AWARENESS",
            "engagement" => "OUTCOME_ENGAGEMENT",
            "app_promotion" => "OUTCOME_APP_PROMOTION",
            _ => "OUTCOME_SALES",
        }
    }

    fn map_objective_from_meta(raw: &str) -> Objective {
        let domain_key = match raw {
            "OUTCOME_SALES" => "sales",
            "OUTCOME_LEADS" => "leads",
            "OUTCOME_TRAFFIC" => "traffic",
            "OUTCOME_AWARENESS" => "awareness",
            "OUTCOME_ENGAGEMENT" => "engagement",
            "OUTCOME_APP_PROMOTION" => "app_promotion",
            other => other,
        };
        Objective::parse(domain_key).unwrap_or_else(|_| Objective::parse("sales").unwrap())
    }

    fn map_status_from_meta(raw: &str) -> CampaignStatus {
        match raw {
            "ACTIVE" => CampaignStatus::Active,
            "PAUSED" => CampaignStatus::Paused,
            "ARCHIVED" => CampaignStatus::Archived,
            "DELETED" => CampaignStatus::Ended,
            _ => CampaignStatus::Paused,
        }
    }
}

#[async_trait]
impl AdPlatformPort for MetaAdsAdapter {
    fn descriptor(&self) -> PlatformDescriptor {
        self.descriptor.clone()
    }

    fn required_scopes(&self) -> Vec<String> {
        vec![
            META_SCOPE_ADS_MANAGEMENT.to_string(),
            META_SCOPE_ADS_READ.to_string(),
            META_SCOPE_BUSINESS_MANAGEMENT.to_string(),
        ]
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

        let scopes = format!(
            "{},{},{}",
            META_SCOPE_ADS_MANAGEMENT, META_SCOPE_ADS_READ, META_SCOPE_BUSINESS_MANAGEMENT
        );

        let params = MetaAuthQueryParams {
            client_id: &self.client_id,
            redirect_uri: &request.redirect_uri,
            response_type: "code",
            scope: &scopes,
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

        let token_url = format!("{}/oauth/access_token", self.graph_base_url);
        let short_lived_resp = self
            .http
            .get(&token_url)
            .query(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("redirect_uri", exchange.redirect_uri.as_str()),
                ("code", exchange.code.as_str()),
            ])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta code exchange transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta code exchange transport failed".to_string(),
                )
            })?;

        if !short_lived_resp.status().is_success() {
            let status = short_lived_resp.status();
            tracing::warn!(status = %status, "meta code exchange returned non-success");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta code exchange failed with status {status}"
            )));
        }

        let short_lived: MetaTokenResponse = short_lived_resp.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta code exchange JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid token response shape from meta".to_string(),
            )
        })?;

        if let Some(err) = short_lived.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported token error: {}",
                err.message
            )));
        }

        let long_lived_resp = self
            .http
            .get(&token_url)
            .query(&[
                ("grant_type", "fb_exchange_token"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("fb_exchange_token", short_lived.access_token.as_str()),
            ])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta long-lived exchange transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta long-lived exchange transport failed".to_string(),
                )
            })?;

        if !long_lived_resp.status().is_success() {
            let status = long_lived_resp.status();
            tracing::warn!(status = %status, "meta long-lived exchange returned non-success");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta long-lived exchange failed with status {status}"
            )));
        }

        let long_lived: MetaTokenResponse = long_lived_resp.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta long-lived exchange JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid long-lived token response from meta".to_string(),
            )
        })?;

        if let Some(err) = long_lived.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported long-lived token error: {}",
                err.message
            )));
        }

        let refresh_token = long_lived.access_token;
        let expires_at = long_lived
            .expires_in
            .map(|secs| Utc::now() + chrono::Duration::seconds(secs));

        let accounts = self.fetch_ad_accounts(&refresh_token).await?;

        Ok(ExchangedAuthorization {
            refresh_token,
            granted_scopes: self.required_scopes(),
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

        let access_token = self.access_tokens.access_token(connection).await?;
        self.fetch_ad_accounts(&access_token).await
    }

    async fn create_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign: Campaign,
        mutation_id: Uuid,
    ) -> Result<Campaign, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let act_id = require_act_id(connection)?;
        let url = format!("{}/{act_id}/campaigns", self.graph_base_url);

        let objective_str = Self::map_objective_to_meta(&campaign.objective);
        let status_str = if campaign.status == CampaignStatus::Active {
            "ACTIVE"
        } else {
            "PAUSED"
        };

        let mut form: Vec<(&str, String)> = vec![
            ("name", campaign.name.clone()),
            ("objective", objective_str.to_string()),
            ("status", status_str.to_string()),
            ("special_ad_categories", "[\"NONE\"]".to_string()),
            ("client_mutation_id", mutation_id.to_string()),
        ];

        match campaign.budget.kind {
            BudgetType::Daily => {
                form.push((
                    "daily_budget",
                    campaign.budget.amount.amount_minor.to_string(),
                ));
            }
            BudgetType::Lifetime => {
                form.push((
                    "lifetime_budget",
                    campaign.budget.amount.amount_minor.to_string(),
                ));
            }
        }

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .form(&form)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta create_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta create_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta create_campaign failed with status {status}"
            )));
        }

        let resp: MetaIdResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta create_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta create_campaign".to_string(),
            )
        })?;

        let mut created = campaign;
        created.id = resp.id;
        Ok(created)
    }

    async fn get_campaign(
        &self,
        connection: &ConnectionHandle,
        campaign_id: &str,
    ) -> Result<Option<Campaign>, AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let url = format!("{}/{campaign_id}", self.graph_base_url);

        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .query(&[(
                "fields",
                "id,name,objective,status,daily_budget,lifetime_budget",
            )])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta get_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta get_campaign transport failed".to_string(),
                )
            })?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            let status = response.status();
            if let Ok(err_envelope) = response.json::<MetaErrorEnvelope>().await {
                if err_envelope.error.code == 100
                    || err_envelope
                        .error
                        .message
                        .to_ascii_lowercase()
                        .contains("does not exist")
                {
                    return Ok(None);
                }
                return Err(AdvertisingError::PlatformUnavailable(format!(
                    "meta reported error: {}",
                    err_envelope.error.message
                )));
            }
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta get_campaign failed with status {status}"
            )));
        }

        let node: MetaCampaignNode = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta get_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta get_campaign".to_string(),
            )
        })?;

        let (budget_kind, amount_minor) = if let Some(daily) = node.daily_budget {
            (BudgetType::Daily, daily.parse::<i64>().unwrap_or(0))
        } else if let Some(lifetime) = node.lifetime_budget {
            (BudgetType::Lifetime, lifetime.parse::<i64>().unwrap_or(0))
        } else {
            (BudgetType::Daily, 0)
        };

        let objective = node
            .objective
            .map(|o| Self::map_objective_from_meta(&o))
            .unwrap_or_else(|| Objective::parse("sales").unwrap());
        let status = node
            .status
            .map(|s| Self::map_status_from_meta(&s))
            .unwrap_or(CampaignStatus::Paused);

        Ok(Some(Campaign {
            id: node.id,
            name: node.name,
            status,
            objective,
            budget: Budget {
                kind: budget_kind,
                amount: Money::new(amount_minor, "JPY")
                    .unwrap_or_else(|_| Money::new(0, "JPY").unwrap()),
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
        let url = format!("{}/{}", self.graph_base_url, campaign.id);

        let status_str = if campaign.status == CampaignStatus::Active {
            "ACTIVE"
        } else {
            "PAUSED"
        };

        let mut form: Vec<(&str, String)> = vec![
            ("name", campaign.name.clone()),
            ("status", status_str.to_string()),
            ("client_mutation_id", mutation_id.to_string()),
        ];

        match campaign.budget.kind {
            BudgetType::Daily => {
                form.push((
                    "daily_budget",
                    campaign.budget.amount.amount_minor.to_string(),
                ));
            }
            BudgetType::Lifetime => {
                form.push((
                    "lifetime_budget",
                    campaign.budget.amount.amount_minor.to_string(),
                ));
            }
        }

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .form(&form)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta update_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta update_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta update_campaign failed with status {status}"
            )));
        }

        let resp: MetaSuccessResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta update_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta update_campaign".to_string(),
            )
        })?;

        if let Some(err) = resp.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported error: {}",
                err.message
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
        let url = format!("{}/{campaign_id}", self.graph_base_url);

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .form(&[("status", "PAUSED")])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta pause_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta pause_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta pause_campaign failed with status {status}"
            )));
        }

        let resp: MetaSuccessResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta pause_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta pause_campaign".to_string(),
            )
        })?;

        if let Some(err) = resp.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported error: {}",
                err.message
            )));
        }

        Ok(Campaign {
            id: campaign_id.to_string(),
            name: "Meta Campaign".to_string(),
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
        let url = format!("{}/{campaign_id}", self.graph_base_url);

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .form(&[("status", "ACTIVE")])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta resume_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta resume_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta resume_campaign failed with status {status}"
            )));
        }

        let resp: MetaSuccessResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta resume_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta resume_campaign".to_string(),
            )
        })?;

        if let Some(err) = resp.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported error: {}",
                err.message
            )));
        }

        Ok(Campaign {
            id: campaign_id.to_string(),
            name: "Meta Campaign".to_string(),
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
        let url = format!("{}/{campaign_id}", self.graph_base_url);

        let response = self
            .http
            .delete(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta delete_campaign transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta delete_campaign transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta delete_campaign failed with status {status}"
            )));
        }

        let resp: MetaSuccessResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta delete_campaign JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta delete_campaign".to_string(),
            )
        })?;

        if let Some(err) = resp.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported error: {}",
                err.message
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
        let act_id = require_act_id(connection)?;
        let url = format!("{}/{act_id}/insights", self.graph_base_url);

        let time_range_json = format!(
            "{{\"since\":\"{}\",\"until\":\"{}\"}}",
            request.from, request.to
        );

        let response = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .query(&[
                ("level", "campaign"),
                ("time_increment", "1"),
                ("time_range", &time_range_json),
                (
                    "fields",
                    "campaign_id,date_start,date_stop,impressions,clicks,spend,actions,action_values,account_currency",
                ),
            ])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta fetch_metrics transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta fetch_metrics transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta insights query failed with status {status}"
            )));
        }

        let body: MetaInsightsResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta fetch_metrics JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid response shape from meta insights query".to_string(),
            )
        })?;

        if let Some(err) = body.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta reported error: {}",
                err.message
            )));
        }

        let mut points = Vec::new();
        for row in body.data {
            let impressions = row
                .impressions
                .as_deref()
                .unwrap_or("0")
                .parse::<i64>()
                .unwrap_or(0);
            let clicks = row
                .clicks
                .as_deref()
                .unwrap_or("0")
                .parse::<i64>()
                .unwrap_or(0);
            let currency = row
                .account_currency
                .unwrap_or_else(|| "JPY".to_string())
                .to_ascii_uppercase();

            let spend_decimal = row
                .spend
                .as_deref()
                .unwrap_or("0")
                .parse::<f64>()
                .unwrap_or(0.0);
            let spend_minor = if currency == "JPY" || currency == "KRW" {
                spend_decimal.round() as i64
            } else {
                (spend_decimal * 100.0).round() as i64
            };

            let conversions: f64 = row
                .actions
                .as_ref()
                .map(|acts| {
                    acts.iter()
                        .map(|a| a.value.parse::<f64>().unwrap_or(0.0))
                        .sum()
                })
                .unwrap_or(0.0);

            let conv_val_decimal: f64 = row
                .action_values
                .as_ref()
                .map(|acts| {
                    acts.iter()
                        .map(|a| a.value.parse::<f64>().unwrap_or(0.0))
                        .sum()
                })
                .unwrap_or(0.0);

            let conv_val_minor = if currency == "JPY" || currency == "KRW" {
                conv_val_decimal.round() as i64
            } else {
                (conv_val_decimal * 100.0).round() as i64
            };

            points.push(MetricPoint {
                campaign_id: row.campaign_id,
                date: row.date_start,
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

    async fn upload(
        &self,
        connection: &ConnectionHandle,
        conversion: UploadedConversion,
        ids: HashedIds,
        consent: ConsentSnapshot,
    ) -> Result<(), AdvertisingError> {
        let access_token = self.access_tokens.access_token(connection).await?;
        let dataset_id = require_act_id(connection)?;
        let dataset_id = strip_act_prefix(&dataset_id).to_string();
        let url = format!("{}/{dataset_id}/events", self.graph_base_url);

        let permitted = !consent
            .answers
            .values()
            .any(|ans| ans.eq_ignore_ascii_case("denied") || ans.eq_ignore_ascii_case("opt_out"));

        let mut user_data = serde_json::Map::new();
        if permitted {
            for (key, val) in ids.values {
                let normalized_key = match key.to_ascii_lowercase().as_str() {
                    "em" | "mail_hash" | "hashed_mail" => "em".to_string(),
                    "ph" | "phone_hash" | "hashed_phone" => "ph".to_string(),
                    "fn" | "first_name" => "fn".to_string(),
                    "ln" | "last_name" => "ln".to_string(),
                    other => other.to_string(),
                };
                user_data.insert(
                    normalized_key,
                    serde_json::Value::Array(vec![serde_json::Value::String(val)]),
                );
            }
        }

        let payload = serde_json::json!({
            "data": [
                {
                    "event_name": "Purchase",
                    "event_time": Utc::now().timestamp(),
                    "event_id": conversion.event_id,
                    "action_source": "website",
                    "user_data": serde_json::Value::Object(user_data)
                }
            ]
        });

        let response = self
            .http
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .json(&payload)
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta conversions upload transport failed");
                AdvertisingError::PlatformUnavailable(
                    "meta conversions upload transport failed".to_string(),
                )
            })?;

        if !response.status().is_success() {
            let status = response.status();
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta conversions upload failed with status {status}"
            )));
        }

        Ok(())
    }
}

fn meta_descriptor() -> PlatformDescriptor {
    let objectives = [
        "app_promotion",
        "awareness",
        "engagement",
        "leads",
        "sales",
        "traffic",
    ]
    .iter()
    .map(|value| Objective::parse(value).expect("static objective"))
    .collect::<BTreeSet<_>>();
    let dimensions = [
        "age",
        "custom_audiences",
        "gender",
        "geo",
        "interests",
        "languages",
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
        BTreeMap::from([("headline".to_string(), 40), ("body".to_string(), 90)]),
    );
    text_limits.insert(
        "ja".to_string(),
        BTreeMap::from([("headline".to_string(), 25), ("body".to_string(), 45)]),
    );

    PlatformDescriptor {
        key: PlatformKey::parse("meta").expect("static key"),
        display_name: "Meta Ads".to_string(),
        entitlement_key: "advertising.meta_ads".to_string(),
        capability_matrix: CapabilityMatrix {
            matrix_version: "2026-08-24".to_string(),
            objectives,
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
                        min_width_px: Some(600),
                        min_height_px: Some(600),
                        max_file_size_bytes: Some(31_457_280),
                        min_duration_seconds: Some(1),
                        max_duration_seconds: Some(14460),
                    },
                },
                CreativePlacement {
                    key: "stories".to_string(),
                    asset_spec: CreativeAssetSpec {
                        image_required: true,
                        video_allowed: true,
                        aspect_ratios: vec!["9:16".to_string()],
                        min_width_px: Some(500),
                        min_height_px: Some(888),
                        max_file_size_bytes: Some(31_457_280),
                        min_duration_seconds: Some(1),
                        max_duration_seconds: Some(120),
                    },
                },
                CreativePlacement {
                    key: "reels".to_string(),
                    asset_spec: CreativeAssetSpec {
                        image_required: false,
                        video_allowed: true,
                        aspect_ratios: vec!["9:16".to_string()],
                        min_width_px: Some(500),
                        min_height_px: Some(888),
                        max_file_size_bytes: Some(31_457_280),
                        min_duration_seconds: Some(3),
                        max_duration_seconds: Some(900),
                    },
                },
            ],
            text_limits,
        },
    }
}

/// Meta's refresh-token grant (`fb_exchange_token` flow).
pub struct MetaRefreshTokenGrant {
    http: reqwest::Client,
    client_id: String,
    client_secret: String,
    graph_base_url: String,
}

impl std::fmt::Debug for MetaRefreshTokenGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaRefreshTokenGrant")
            .field("client_id", &"[REDACTED]")
            .field("client_secret", &"[REDACTED]")
            .field("graph_base_url", &self.graph_base_url)
            .finish()
    }
}

impl MetaRefreshTokenGrant {
    pub fn new(config: &AdvertisingConfig) -> Result<Self, AdvertisingError> {
        let api_version = if config.meta_api_version.trim().is_empty() {
            "v24.0".to_string()
        } else {
            config.meta_api_version.trim().to_string()
        };
        let graph_base_url = format!("https://graph.facebook.com/{api_version}");
        Self::with_graph_base_url(config, graph_base_url)
    }

    pub fn with_graph_base_url(
        config: &AdvertisingConfig,
        graph_base_url: String,
    ) -> Result<Self, AdvertisingError> {
        let client_id = config.meta_client_id.trim().to_string();
        if client_id.is_empty() {
            return Err(AdvertisingError::PlatformUnavailable(
                "meta client id is required".to_string(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| AdvertisingError::PlatformUnavailable(err.to_string()))?;
        Ok(Self {
            http,
            client_id,
            client_secret: config.meta_client_secret.clone(),
            graph_base_url,
        })
    }
}

#[async_trait]
impl RefreshTokenGrant for MetaRefreshTokenGrant {
    async fn exchange_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<GrantedAccessToken, AdvertisingError> {
        if refresh_token.trim().is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "refresh token is required".to_string(),
            ));
        }

        let token_url = format!("{}/oauth/access_token", self.graph_base_url);
        let response = self
            .http
            .get(&token_url)
            .query(&[
                ("grant_type", "fb_exchange_token"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("fb_exchange_token", refresh_token),
            ])
            .send()
            .await
            .map_err(|err| {
                tracing::warn!(error = %err, "meta refresh grant transport failed");
                AdvertisingError::PlatformUnavailable("meta oauth refresh grant failed".to_string())
            })?;

        if !response.status().is_success() {
            let status = response.status();
            tracing::warn!(status = %status, "meta refresh grant returned non-success");
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta oauth refresh grant failed with status {status}"
            )));
        }

        let token_resp: MetaTokenResponse = response.json().await.map_err(|err| {
            tracing::warn!(error = %err, "meta refresh grant JSON parse failed");
            AdvertisingError::PlatformUnavailable(
                "invalid refresh grant response from meta".to_string(),
            )
        })?;

        if let Some(err) = token_resp.error {
            return Err(AdvertisingError::PlatformUnavailable(format!(
                "meta oauth refresh grant error: {}",
                err.message
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
        #[allow(dead_code)]
        method: String,
        path: String,
        authorization: Option<String>,
        #[allow(dead_code)]
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
                            let mut content_length = 0usize;
                            for line in lines {
                                if let Some((k, v)) = line.split_once(':') {
                                    let k = k.trim().to_ascii_lowercase();
                                    let v = v.trim().to_string();
                                    if k == "authorization" {
                                        authorization = Some(v);
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
            meta_client_id: "test_meta_client_123".to_string(),
            meta_client_secret: "test_meta_secret_456".to_string(),
            meta_api_version: "v24.0".to_string(),
            ..AdvertisingConfig::default()
        }
    }

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
    fn construction_fails_cleanly_on_empty_client_id() {
        let config = AdvertisingConfig {
            meta_client_id: "   ".to_string(),
            ..test_config()
        };
        let err = MetaAdsAdapter::new(&config, stub_tokens("eaab.unused"))
            .expect_err("empty client id must fail");
        assert!(matches!(err, AdvertisingError::PlatformUnavailable(_)));
    }

    #[test]
    fn debug_output_redacts_credentials() {
        let adapter = MetaAdsAdapter::new(&test_config(), stub_tokens("eaab.unused")).unwrap();
        let debug = format!("{adapter:?}");
        assert!(!debug.contains("test_meta_client_123"));
        assert!(!debug.contains("test_meta_secret_456"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn descriptor_and_scopes_are_correct() {
        let adapter = MetaAdsAdapter::new(&test_config(), stub_tokens("eaab.unused")).unwrap();
        assert_eq!(adapter.descriptor().key.as_str(), "meta");
        assert_eq!(
            adapter.required_scopes(),
            vec![
                "ads_management".to_string(),
                "ads_read".to_string(),
                "business_management".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn begin_authorization_constructs_valid_meta_oauth_url() {
        let adapter = MetaAdsAdapter::new(&test_config(), stub_tokens("eaab.unused")).unwrap();
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
        assert!(
            start
                .authorization_url
                .starts_with("https://www.facebook.com/v24.0/dialog/oauth")
        );
        assert!(
            start
                .authorization_url
                .contains("client_id=test_meta_client_123")
        );
        assert!(start.authorization_url.contains("response_type=code"));
        assert!(
            start
                .authorization_url
                .contains("scope=ads_management%2Cads_read%2Cbusiness_management")
        );
        assert!(start.authorization_url.contains("state=test-nonce-state"));
    }

    #[tokio::test]
    async fn exchange_authorization_exchanges_tokens_and_lists_accounts() {
        let short_lived = serde_json::json!({
            "access_token": "short_lived_meta_token",
            "token_type": "bearer",
            "expires_in": 3600
        });
        let long_lived = serde_json::json!({
            "access_token": "long_lived_meta_refresh_token",
            "token_type": "bearer",
            "expires_in": 5184000
        });
        let accounts_json = serde_json::json!({
            "data": [
                { "id": "act_1001001", "name": "Main Meta Account", "account_status": 1 },
                { "id": "act_2002002", "name": "Secondary Account", "account_status": 1 }
            ]
        });

        let mock = MockServer::start(vec![
            (200, short_lived),
            (200, long_lived),
            (200, accounts_json),
        ])
        .await;

        let adapter = MetaAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("eaab.provider_token"),
        )
        .unwrap();

        let exchange = AuthorizationExchange {
            code: "meta-auth-code-123".to_string(),
            redirect_uri: "https://tenant.example/callback".to_string(),
        };

        let res = adapter
            .exchange_authorization(&exchange)
            .await
            .expect("exchange succeeds");

        assert_eq!(res.refresh_token, "long_lived_meta_refresh_token");
        assert_eq!(res.accounts.len(), 2);
        assert_eq!(res.accounts[0].external_id, "1001001");
        assert_eq!(res.accounts[0].display_name, "Main Meta Account");
        assert_eq!(res.accounts[1].external_id, "2002002");

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 3);
        assert!(recorded[0].path.contains("code=meta-auth-code-123"));
        assert!(recorded[1].path.contains("grant_type=fb_exchange_token"));
        assert!(recorded[2].path.contains("/me/adaccounts"));
    }

    #[tokio::test]
    async fn list_accounts_queries_graph_api() {
        let accounts_json = serde_json::json!({
            "data": [
                { "id": "act_999888", "name": "Listed Account", "account_status": 1 }
            ]
        });
        let mock = MockServer::start(vec![(200, accounts_json)]).await;

        let adapter = MetaAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("eaab.provider_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            id: "conn-123".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "conn-123".to_string(),
        };

        let accounts = adapter
            .list_accounts(&connection)
            .await
            .expect("list accounts succeeds");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].external_id, "999888");
        assert_eq!(accounts[0].display_name, "Listed Account");

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].authorization.as_deref(),
            Some("Bearer eaab.provider_token")
        );
    }

    #[tokio::test]
    async fn account_scoped_calls_address_the_external_account_not_the_connection_id() {
        let mock = MockServer::start(vec![(
            200,
            serde_json::json!({ "id": "23847", "status": "PAUSED" }),
        )])
        .await;
        let adapter = MetaAdsAdapter::with_custom_endpoints(
            &test_config(),
            "https://www.facebook.example/dialog/oauth".to_string(),
            format!("http://{}", mock.addr),
            stub_tokens("eaab.provider_issued_access_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            id: "conn-9f2c7d".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "1001001".to_string(),
        };

        let _ = adapter
            .fetch_metrics(
                &connection,
                &MetricQuery {
                    from: "2026-08-05".to_string(),
                    to: "2026-08-05".to_string(),
                },
            )
            .await;

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0].path.contains("/act_1001001/insights"),
            "expected the external ad account in {}",
            recorded[0].path
        );
        assert!(
            !recorded[0].path.contains("conn-9f2c7d"),
            "the Sanvi connection id must never reach the platform: {}",
            recorded[0].path
        );
    }

    #[tokio::test]
    async fn account_scoped_calls_refuse_a_handle_with_no_external_account() {
        let adapter = MetaAdsAdapter::new(&test_config(), stub_tokens("eaab.unused")).unwrap();
        let connection = ConnectionHandle {
            id: "conn-9f2c7d".to_string(),
            tenant_id: test_tenant(),
            external_account_id: String::new(),
        };

        // No mock server is running: reaching the network at all would fail
        // differently, so `InvalidInput` proves the guard rejected first.
        assert!(matches!(
            adapter
                .fetch_metrics(
                    &connection,
                    &MetricQuery {
                        from: "2026-08-05".to_string(),
                        to: "2026-08-05".to_string(),
                    },
                )
                .await,
            Err(AdvertisingError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn campaign_crud_and_metrics_against_mock() {
        let create_resp = serde_json::json!({ "id": "120330001" });
        let get_resp = serde_json::json!({
            "id": "120330001",
            "name": "Summer Sales",
            "objective": "OUTCOME_SALES",
            "status": "ACTIVE",
            "daily_budget": "5000"
        });
        let update_resp = serde_json::json!({ "success": true });
        let pause_resp = serde_json::json!({ "success": true });
        let resume_resp = serde_json::json!({ "success": true });
        let delete_resp = serde_json::json!({ "success": true });
        let insights_resp = serde_json::json!({
            "data": [
                {
                    "campaign_id": "120330001",
                    "date_start": "2026-08-01",
                    "date_stop": "2026-08-01",
                    "impressions": "1500",
                    "clicks": "45",
                    "spend": "5000",
                    "account_currency": "JPY",
                    "actions": [
                        { "action_type": "purchase", "value": "3.5" }
                    ],
                    "action_values": [
                        { "action_type": "purchase", "value": "12500" }
                    ]
                }
            ]
        });
        let upload_resp = serde_json::json!({ "events_received": 1 });

        let mock = MockServer::start(vec![
            (200, create_resp),
            (200, get_resp),
            (200, update_resp),
            (200, pause_resp),
            (200, resume_resp),
            (200, delete_resp),
            (200, insights_resp),
            (200, upload_resp),
        ])
        .await;

        let adapter = MetaAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("eaab.provider_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            // Deliberately unlike the account id: the platform only ever hears
            // about `external_account_id`, never Sanvi's own connection id.
            id: "conn-9f2c7d".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "1001001".to_string(),
        };

        let campaign = Campaign {
            id: "draft-c-1".to_string(),
            name: "Summer Sales".to_string(),
            status: CampaignStatus::Active,
            objective: Objective::parse("sales").unwrap(),
            budget: Budget {
                kind: BudgetType::Daily,
                amount: Money::new(5000, "JPY").unwrap(),
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
        assert_eq!(created.id, "120330001");

        // 2. Get
        let fetched = adapter
            .get_campaign(&connection, "120330001")
            .await
            .expect("get campaign succeeds")
            .expect("campaign found");
        assert_eq!(fetched.id, "120330001");
        assert_eq!(fetched.name, "Summer Sales");
        assert_eq!(fetched.status, CampaignStatus::Active);
        assert_eq!(fetched.budget.amount.amount_minor, 5000);

        // 3. Update
        let updated = adapter
            .update_campaign(&connection, fetched, Uuid::now_v7())
            .await
            .expect("update campaign succeeds");
        assert_eq!(updated.id, "120330001");

        // 4. Pause
        let paused = adapter
            .pause_campaign(&connection, "120330001")
            .await
            .expect("pause campaign succeeds");
        assert_eq!(paused.status, CampaignStatus::Paused);

        // 5. Resume
        let resumed = adapter
            .resume_campaign(&connection, "120330001")
            .await
            .expect("resume campaign succeeds");
        assert_eq!(resumed.status, CampaignStatus::Active);

        // 6. Delete
        adapter
            .delete_campaign(&connection, "120330001")
            .await
            .expect("delete campaign succeeds");

        // 7. Metrics
        let metrics = adapter
            .fetch_metrics(
                &connection,
                &MetricQuery {
                    from: "2026-08-01".to_string(),
                    to: "2026-08-01".to_string(),
                },
            )
            .await
            .expect("fetch metrics succeeds");
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].impressions, 1500);
        assert_eq!(metrics[0].clicks, 45);
        assert_eq!(metrics[0].spend.amount_minor, 5000);
        assert_eq!(metrics[0].conversions, 3.5);
        assert_eq!(metrics[0].conversion_value.amount_minor, 12500);

        // 8. Upload
        adapter
            .upload(
                &connection,
                UploadedConversion {
                    event_id: "evt-123".to_string(),
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
    async fn get_campaign_returns_none_on_404_or_missing_code() {
        let mock = MockServer::start(vec![(
            404,
            serde_json::json!({
                "error": {
                    "message": "(#100) Object does not exist",
                    "code": 100
                }
            }),
        )])
        .await;

        let adapter = MetaAdsAdapter::with_custom_endpoints(
            &test_config(),
            format!("http://{}/auth", mock.addr),
            format!("http://{}", mock.addr),
            stub_tokens("eaab.provider_token"),
        )
        .unwrap();

        let connection = ConnectionHandle {
            // Deliberately unlike the account id: the platform only ever hears
            // about `external_account_id`, never Sanvi's own connection id.
            id: "conn-9f2c7d".to_string(),
            tenant_id: test_tenant(),
            external_account_id: "1001001".to_string(),
        };

        let result = adapter
            .get_campaign(&connection, "missing-id")
            .await
            .expect("missing returns Ok(None)");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn refresh_grant_trades_refresh_token_for_access_token() {
        let token_fixture = serde_json::json!({
            "access_token": "fresh_meta_access_token",
            "token_type": "bearer",
            "expires_in": 5184000
        });
        let mock = MockServer::start(vec![(200, token_fixture)]).await;

        let grant = MetaRefreshTokenGrant::with_graph_base_url(
            &test_config(),
            format!("http://{}", mock.addr),
        )
        .unwrap();

        let granted = grant
            .exchange_refresh_token("existing_refresh_token")
            .await
            .expect("refresh grant succeeds");
        assert_eq!(granted.access_token, "fresh_meta_access_token");
        assert!(granted.expires_at.is_some());

        let recorded = mock.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].path.contains("grant_type=fb_exchange_token"));
        assert!(
            recorded[0]
                .path
                .contains("fb_exchange_token=existing_refresh_token")
        );
    }

    #[test]
    fn refresh_grant_debug_redacts_credentials() {
        let grant = MetaRefreshTokenGrant::new(&test_config()).unwrap();
        let debug = format!("{grant:?}");
        assert!(!debug.contains("test_meta_client_123"));
        assert!(!debug.contains("test_meta_secret_456"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn refresh_grant_rejects_an_empty_refresh_token_without_a_call() {
        let mock = MockServer::start(vec![]).await;
        let grant = MetaRefreshTokenGrant::with_graph_base_url(
            &test_config(),
            format!("http://{}", mock.addr),
        )
        .unwrap();

        let err = grant
            .exchange_refresh_token("   ")
            .await
            .expect_err("empty refresh token is rejected");
        assert!(matches!(err, AdvertisingError::InvalidInput(_)));
        assert!(mock.recorded().is_empty());
    }
}
