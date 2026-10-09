use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use sanvi_kernel::{Id, TenantId, UserId};
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

pub enum AudienceTag {}
pub type AudienceId = Id<AudienceTag>;

pub enum BudgetCapTag {}
pub type BudgetCapId = Id<BudgetCapTag>;

pub enum BudgetAlertTag {}
pub type BudgetAlertId = Id<BudgetAlertTag>;

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
    /// Campaign-budget optimization: one shared budget on the campaign caps
    /// spend across all of its ad sets, instead of each ad set carrying its
    /// own. Only platforms that declare it in their `CapabilityMatrix` accept
    /// it — validation rejects it elsewhere on the `budget.kind` path. The
    /// platform mapping lives in the adapter descriptors, never here.
    CampaignBudgetOptimization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = AdvertisingMoney)]
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

    /// Minor-unit exponent for an ISO-4217 code: 0 for zero-decimal
    /// currencies (JPY, KRW, …), 3 for three-decimal ones (BHD, KWD, …), 2
    /// for everything else. FX rates convert *major* units, so any
    /// minor-unit conversion must divide out the source exponent and multiply
    /// in the target's — multiplying minor units by the bare rate is 100×
    /// off across a 2↔0 boundary.
    pub fn exponent(currency: &str) -> u32 {
        match currency.trim().to_ascii_uppercase().as_str() {
            "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF"
            | "UGX" | "UYI" | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0,
            "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
            _ => 2,
        }
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

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AdReviewState {
    Pending,
    Approved,
    Rejected,
}

/// The platform's review verdict for one ad. `reasons` preserves the
/// platform's own policy wording verbatim — a paraphrased rejection is worse
/// than useless when the tenant has to fix the ad in the native tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AdReview {
    pub state: AdReviewState,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub checked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for AdReview {
    fn default() -> Self {
        Self {
            state: AdReviewState::Pending,
            reasons: Vec::new(),
            checked_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Ad {
    pub id: String,
    /// The platform's own id for this ad, once one has been recorded — the
    /// only key that correlates a review observation (which arrives keyed by
    /// platform id) back onto this ad. `None` for ads the platform has never
    /// echoed an id for; a platform that assigns ids at creation must persist
    /// them here, or its verdicts can never be matched.
    #[serde(default)]
    pub external_id: Option<String>,
    pub creative: Creative,
    pub landing_url: String,
    pub tracking_template: Option<String>,
    /// Last review observation pulled from the platform. Absent (pending,
    /// never checked) until a poll happens — ads only get reviewed once they
    /// are published.
    #[serde(default)]
    pub review: AdReview,
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
    Publishing,
    Active,
    Paused,
    Ended,
    Archived,
}

impl CampaignStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Publishing => "publishing",
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
    /// The owning tenant travels with the handle so a shared adapter can reach
    /// tenant-scoped state (the OAuth token vault) without a cross-tenant read.
    /// Without it a shared `AdPlatformPort` would have to resolve the tenant by
    /// connection id alone, which means an RLS bypass on a live request path.
    #[schema(value_type = i64)]
    pub tenant_id: TenantId,
    /// The *platform's* own account identifier for this connection, in whatever
    /// shape that platform's adapter expects. It travels with the handle for the
    /// same reason `tenant_id` does: every account-scoped adapter call addresses
    /// this id, and `id` above is the Sanvi `ConnectionId`, which no platform has
    /// ever heard of. Resolving it inside an adapter would mean a tenant-scoped
    /// read no `AdPlatformPort` method carries a scope for.
    ///
    /// Empty **only** on the transient handle the authorization endpoints use
    /// before any account has been selected (see
    /// `application::connections::transient_handle`). An adapter method that
    /// addresses an account must reject an empty value rather than build a URL
    /// around it.
    #[serde(default)]
    pub external_account_id: String,
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

/// Per-platform upload progress. Suppression is deliberately absent: a
/// directive suppresses the whole event, not one platform, and the frozen
/// `ConsentSnapshot` already carries which purpose was denied and by which
/// signal source. Duplicating that here would give suppression two homes
/// that can disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UploadState {
    Pending {
        #[serde(default)]
        attempt_count: u32,
    },
    Uploaded {
        #[serde(default)]
        attempt_count: u32,
    },
    Failed {
        reason: String,
        #[serde(default)]
        attempt_count: u32,
    },
    Parked {
        reason: String,
        attempts: u32,
    },
    Retracted,
    Unpropagated {
        reason: String,
    },
}

impl Default for UploadState {
    fn default() -> Self {
        Self::Pending { attempt_count: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UploadReceipt {
    pub platform: PlatformKey,
    pub external_id: String,
    pub uploaded_at: DateTime<Utc>,
}

/// Enough of the capturing subject's identity to rebuild the exact
/// `SubjectRef` a later withdrawal re-check must query — without ever
/// persisting the raw email/phone TASK-014 forbids in this context. A
/// device ref, a tenant's own customer ref, and a platform user id are not
/// themselves the raw PII that rule targets, so they are stored verbatim;
/// an email-identified (`Contact`) subject stores nothing reconstructable,
/// and the retry path refuses rather than re-deriving a directive answer
/// against the wrong subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SubjectKeyRef {
    TenantDevice {
        device_ref: String,
    },
    EndUser {
        external_id: String,
    },
    TenantUser {
        user_id: i64,
    },
    /// Captured under an email-identified subject. No key material is
    /// stored; a later withdrawal re-check has nothing safe to query and
    /// must refuse rather than guess.
    Contact,
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
    #[serde(default)]
    pub upload_states: BTreeMap<PlatformKey, UploadState>,
    #[serde(default = "default_subject_key")]
    pub subject_key: SubjectKeyRef,
}

fn default_subject_key() -> SubjectKeyRef {
    SubjectKeyRef::Contact
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ConversionDiagnostics {
    pub event: ConversionEvent,
    pub captured_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AudienceStatus {
    Building,
    Active,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Audience {
    #[schema(value_type = String)]
    pub id: AudienceId,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    pub platform: PlatformKey,
    pub name: String,
    pub status: AudienceStatus,
    pub external_ref: Option<String>,
    pub included_identifiers: Vec<HashedIdentifier>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AudienceSyncReceipt {
    pub external_ref: String,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Daily,
    Monthly,
}

impl BudgetPeriod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// First day of the budget period that `today` falls into.
    ///
    /// Two periods are the same period exactly when this returns the same date,
    /// which is what threshold-alert dedupe and the rollover boundary check both
    /// key on.
    pub fn period_start(self, today: NaiveDate) -> NaiveDate {
        match self {
            Self::Daily => today,
            Self::Monthly => {
                NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today)
            }
        }
    }

    pub fn parse(raw: &str) -> Result<Self, AdvertisingError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "daily" => Ok(Self::Daily),
            "monthly" => Ok(Self::Monthly),
            other => Err(AdvertisingError::InvalidInput(format!(
                "invalid budget period: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "kind", content = "campaign_id", rename_all = "snake_case")]
pub enum BudgetScope {
    Tenant,
    Campaign {
        #[schema(value_type = String)]
        campaign_id: CampaignId,
    },
}

impl BudgetScope {
    pub fn as_scope_string(&self) -> String {
        match self {
            Self::Tenant => "tenant".to_string(),
            Self::Campaign { campaign_id } => format!("campaign:{}", campaign_id),
        }
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AlertCondition {
    SettledBreach,
    ProvisionalLowerBoundBreach,
    ProvisionalWarning,
    StaleDataWarning,
    SpendAnomaly,
    Threshold80,
}

impl AlertCondition {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SettledBreach => "settled_breach",
            Self::ProvisionalLowerBoundBreach => "provisional_lower_bound_breach",
            Self::ProvisionalWarning => "provisional_warning",
            Self::StaleDataWarning => "stale_warning",
            Self::SpendAnomaly => "spend_anomaly",
            Self::Threshold80 => "threshold_80",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, AdvertisingError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "settled_breach" => Ok(Self::SettledBreach),
            "provisional_lower_bound_breach" => Ok(Self::ProvisionalLowerBoundBreach),
            "provisional_warning" => Ok(Self::ProvisionalWarning),
            "stale_warning" => Ok(Self::StaleDataWarning),
            "spend_anomaly" => Ok(Self::SpendAnomaly),
            "threshold_80" => Ok(Self::Threshold80),
            other => Err(AdvertisingError::InvalidInput(format!(
                "invalid alert condition: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DataFreshness {
    pub last_synced_at: Option<DateTime<Utc>>,
    pub lag_hours: Option<f64>,
    pub is_settled: bool,
    pub is_stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BudgetCap {
    #[schema(value_type = String)]
    pub id: BudgetCapId,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    #[schema(value_type = Option<String>)]
    pub campaign_id: Option<CampaignId>,
    pub period: BudgetPeriod,
    pub amount: Money,
    pub effective_currency: String,
    pub declared_fx_basis: Option<String>,
    pub fx_rate_date: Option<chrono::NaiveDate>,
    pub auto_pause: bool,
    pub auto_resume_on_rollover: bool,
    pub version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BudgetAlert {
    #[schema(value_type = String)]
    pub id: BudgetAlertId,
    #[schema(value_type = String)]
    pub tenant_id: TenantId,
    #[schema(value_type = Option<String>)]
    pub cap_id: Option<BudgetCapId>,
    #[schema(value_type = Option<String>)]
    pub campaign_id: Option<CampaignId>,
    pub period: BudgetPeriod,
    pub period_start: chrono::NaiveDate,
    pub threshold: u32,
    pub condition: AlertCondition,
    pub spend: Money,
    pub cap_amount: Money,
    pub data_freshness: DataFreshness,
    pub auto_paused: bool,
    pub acknowledged_at: Option<DateTime<Utc>>,
    #[schema(value_type = Option<String>)]
    pub acknowledged_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BudgetActionsConfigured {
    pub threshold_80: String,
    pub threshold_100: String,
    pub auto_pause: bool,
    pub auto_resume_on_rollover: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SpendStatusItem {
    pub scope: String,
    #[schema(value_type = Option<String>)]
    pub campaign_id: Option<CampaignId>,
    pub period: BudgetPeriod,
    pub cap: Option<BudgetCap>,
    pub spend_to_date: Money,
    pub settled_spend: Money,
    pub provisional_spend: Money,
    pub provisional_lower_bound: Option<Money>,
    pub projected_spend: Money,
    pub percentage: Option<f64>,
    pub data_freshness: DataFreshness,
    pub actions_configured: BudgetActionsConfigured,
    /// Currencies for which spend data could not be converted to the cap currency
    /// because an FX rate was unavailable. When non-empty, spend totals are incomplete.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unconvertible_spend_currencies: Vec<String>,
}
