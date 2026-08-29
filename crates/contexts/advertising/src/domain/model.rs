use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use sanvi_kernel::{Id, TenantId};
use serde::{Deserialize, Serialize};

use super::AdvertisingError;
use super::hashing::HashedIdentifier;

pub enum ConnectionTag {}
pub type ConnectionId = Id<ConnectionTag>;

pub enum CampaignTag {}
pub type CampaignId = Id<CampaignTag>;

pub enum CampaignChangeTag {}
pub type CampaignChangeId = Id<CampaignChangeTag>;

pub enum ConversionTag {}
pub type ConversionId = Id<ConversionTag>;

pub enum CreativeTag {}
pub type CreativeId = Id<CreativeTag>;

pub enum AdGroupTag {}
pub type AdGroupId = Id<AdGroupTag>;

pub enum AdTag {}
pub type AdId = Id<AdTag>;

/// Adapter-owned identifier. Domain code never switches on a platform name.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(transparent)]
pub struct PlatformKey(String);

impl PlatformKey {
    pub fn parse(raw: &str) -> Result<Self, AdvertisingError> {
        let normalized = raw.trim().to_ascii_lowercase();
        if normalized.is_empty()
            || !normalized.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
            })
        {
            return Err(AdvertisingError::InvalidInput(
                "platform key must contain lowercase letters, digits, or underscores".to_string(),
            ));
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(transparent)]
pub struct Objective(String);

impl Objective {
    pub fn parse(raw: &str) -> Result<Self, AdvertisingError> {
        let value = raw.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "objective is required".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BudgetType {
    Daily,
    Lifetime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Money {
    pub amount_minor: i64,
    pub currency: String,
}

impl Money {
    pub fn new(amount_minor: i64, currency: &str) -> Result<Self, AdvertisingError> {
        let currency = currency.trim().to_ascii_uppercase();
        if amount_minor < 0
            || currency.len() != 3
            || !currency
                .chars()
                .all(|character| character.is_ascii_alphabetic())
        {
            return Err(AdvertisingError::InvalidInput(
                "money requires non-negative minor units and ISO-4217 currency".to_string(),
            ));
        }
        Ok(Self {
            amount_minor,
            currency,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Budget {
    pub kind: BudgetType,
    pub amount: Money,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Schedule {
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(transparent)]
pub struct TargetingDimension(String);

impl TargetingDimension {
    pub fn parse(raw: &str) -> Result<Self, AdvertisingError> {
        let value = raw.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Err(AdvertisingError::InvalidInput(
                "targeting dimension is required".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Typed extension data. Arbitrary JSON is deliberately not admitted to the domain.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Targeting {
    pub dimensions: BTreeMap<TargetingDimension, Vec<String>>,
    pub extension_fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BidSettings {
    pub strategy: String,
    pub maximum_bid: Option<Money>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CreativeText {
    pub locale: String,
    pub headline: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Creative {
    pub id: String,
    pub placement: String,
    pub texts: Vec<CreativeText>,
    pub asset_references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Ad {
    pub id: String,
    pub creative: Creative,
    pub landing_url: String,
    pub tracking_template: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AdGroup {
    pub id: String,
    pub name: String,
    pub targeting: Targeting,
    pub bid: BidSettings,
    pub ads: Vec<Ad>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DriftState {
    pub drifted: bool,
    pub changed_fields: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CampaignStatus {
    Draft,
    Active,
    Paused,
    Ended,
    Archived,
}

impl CampaignStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Ended => "ended",
            Self::Archived => "archived",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Campaign {
    pub id: String,
    pub name: String,
    pub status: CampaignStatus,
    pub objective: Objective,
    pub budget: Budget,
    pub schedule: Option<Schedule>,
    pub ad_groups: Vec<AdGroup>,
    pub drift: DriftState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ConnectionHandle {
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    NotConnected,
    Connected,
}

/// Only normalized hashes can cross the upload port.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HashedIds {
    pub values: BTreeMap<String, String>,
}

/// Capture-time directive proof required by every outbound upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ConsentSnapshot {
    pub resolver_version: String,
    pub jurisdiction: String,
    pub purposes_asked: Vec<String>,
    pub answers: BTreeMap<String, String>,
    pub signal_source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ClickIds {
    #[serde(default)]
    pub gclid: Option<String>,
    #[serde(default)]
    pub gbraid: Option<String>,
    #[serde(default)]
    pub wbraid: Option<String>,
    #[serde(default)]
    pub fbc: Option<String>,
    #[serde(default)]
    pub fbp: Option<String>,
    #[serde(default)]
    pub capture_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub landing_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    PaymentRecord,
    ClientReported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SuppressedByDirective {
    pub purpose: String,
    pub signal_source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UploadState {
    #[default]
    Pending,
    SuppressedByDirective(SuppressedByDirective),
    Uploaded,
    Failed {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ConversionEvent {
    #[schema(value_type = String)]
    pub id: ConversionId,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    pub event_id: String,
    /// Immutable storage identity. For order-backed events this is derived
    /// from the order reference and event name, so a webhook state change
    /// cannot turn one purchase into two captured rows.
    #[serde(skip)]
    pub dedupe_key: String,
    pub name: String,
    pub occurred_at: DateTime<Utc>,
    pub value: Option<Money>,
    pub value_source: Option<ValueSource>,
    pub order_ref: Option<String>,
    pub click_ids: ClickIds,
    pub hashed_identifiers: BTreeMap<String, HashedIdentifier>,
    pub consent: ConsentSnapshot,
    pub upload_state: UploadState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ConversionDiagnostics {
    pub event: ConversionEvent,
    pub captured_at: DateTime<Utc>,
}
