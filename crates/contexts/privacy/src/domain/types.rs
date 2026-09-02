//! The privacy context's domain vocabulary: subjects, jurisdictions,
//! purposes, directives, data classes and the request aggregates' value
//! types. Nothing here may import infrastructure crates (enforced by
//! `scripts/check-layering.sh`).

use std::fmt;

use chrono::{DateTime, Utc};
use sanvi_kernel::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::PrivacyError;

/// Who the personal data belongs to, relative to the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// A platform user inside a tenant (we are the controller).
    TenantUser,
    /// A tenant's own customer (the tenant is the controller, we are the
    /// processor / service provider).
    EndUser,
    /// A person who contacted the platform without an account (support,
    /// sales, marketing).
    Contact,
    /// A pre-authentication device identity (GPC/UOOM signals).
    Device,
}

impl SubjectKind {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "tenant_user" => Ok(Self::TenantUser),
            "end_user" => Ok(Self::EndUser),
            "contact" => Ok(Self::Contact),
            "device" => Ok(Self::Device),
            other => Err(PrivacyError::InvalidSubjectKind(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TenantUser => "tenant_user",
            Self::EndUser => "end_user",
            Self::Contact => "contact",
            Self::Device => "device",
        }
    }
}

/// One identifier by which a subject can be looked up (email, platform user
/// id, tenant-internal customer id, …).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct SubjectIdentifier {
    #[serde(rename = "type")]
    pub id_type: String,
    pub value: String,
}

/// The cross-context reference to a data subject. `key` is the canonical,
/// stable key used for directive materialisation and crypto-shredding:
/// `user:{snowflake}`, `end_user:{tenant_id}:{external_id}`, `email:{addr}` or
/// `device:{device_ref}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct SubjectRef {
    pub kind: SubjectKind,
    #[schema(value_type = Option<String>)]
    pub tenant_id: Option<TenantId>,
    pub key: String,
    pub identifiers: Vec<SubjectIdentifier>,
}

impl SubjectRef {
    pub fn user(user_id: UserId) -> Self {
        Self {
            kind: SubjectKind::TenantUser,
            tenant_id: None,
            key: format!("user:{user_id}"),
            identifiers: vec![SubjectIdentifier {
                id_type: "user_id".to_string(),
                value: user_id.to_string(),
            }],
        }
    }

    pub fn end_user(tenant_id: TenantId, external_id: &str) -> Self {
        Self {
            kind: SubjectKind::EndUser,
            tenant_id: Some(tenant_id),
            key: format!("end_user:{tenant_id}:{external_id}"),
            identifiers: vec![SubjectIdentifier {
                id_type: "external_id".to_string(),
                value: external_id.to_string(),
            }],
        }
    }

    pub fn contact(email: &str) -> Self {
        Self {
            kind: SubjectKind::Contact,
            tenant_id: None,
            key: format!("email:{}", email.to_ascii_lowercase()),
            identifiers: vec![SubjectIdentifier {
                id_type: "email".to_string(),
                value: email.to_ascii_lowercase(),
            }],
        }
    }

    /// A device seen on one tenant's own site. Device references are
    /// minted by that site, so they are unique only within the tenant —
    /// keying them globally, as [`SubjectRef::device`] does, would let one
    /// tenant's opt-out silence every other tenant's identically-named
    /// device.
    pub fn tenant_device(tenant_id: TenantId, device_ref: &str) -> Self {
        Self {
            kind: SubjectKind::Device,
            tenant_id: Some(tenant_id),
            key: format!("device:{tenant_id}:{device_ref}"),
            identifiers: vec![SubjectIdentifier {
                id_type: "device_ref".to_string(),
                value: device_ref.to_string(),
            }],
        }
    }

    pub fn device(device_ref: &str) -> Self {
        Self {
            kind: SubjectKind::Device,
            tenant_id: None,
            key: format!("device:{device_ref}"),
            identifiers: vec![SubjectIdentifier {
                id_type: "device_ref".to_string(),
                value: device_ref.to_string(),
            }],
        }
    }

    pub fn canonical_key(&self) -> &str {
        &self.key
    }
}

/// A jurisdiction code (`eu`, `uk`, `jp`, `us-ca`, …). Parsed, never
/// free-form, so a typo fails at the boundary rather than silently falling
/// back to the most protective profile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
pub struct JurisdictionRef(String);

impl JurisdictionRef {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        let code = raw.trim().to_ascii_lowercase();
        let valid = code.len() >= 2
            && code.len() <= 32
            && code
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return Err(PrivacyError::InvalidJurisdiction(raw.to_string()));
        }
        Ok(Self(code))
    }

    pub fn from_static(raw: &'static str) -> Self {
        Self(raw.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JurisdictionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The legal regime a jurisdiction belongs to; drives nothing by itself —
/// every decision reads the profile's model fields instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    Gdpr,
    UkGdpr,
    Appi,
    UsState,
}

impl Regime {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "gdpr" => Ok(Self::Gdpr),
            "uk_gdpr" => Ok(Self::UkGdpr),
            "appi" => Ok(Self::Appi),
            "us_state" => Ok(Self::UsState),
            other => Err(PrivacyError::InvalidRegime(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gdpr => "gdpr",
            Self::UkGdpr => "uk_gdpr",
            Self::Appi => "appi",
            Self::UsState => "us_state",
        }
    }
}

/// How non-essential processing may start: permission first (EU/UK/JP) or
/// notice with an honoured refusal (US states).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConsentModel {
    OptIn,
    NoticeAndOptOut,
}

impl ConsentModel {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "opt_in" => Ok(Self::OptIn),
            "notice_and_opt_out" => Ok(Self::NoticeAndOptOut),
            other => Err(PrivacyError::InvalidConsentModel(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptIn => "opt_in",
            Self::NoticeAndOptOut => "notice_and_opt_out",
        }
    }
}

/// How sensitive-personal-information processing starts: permission first,
/// a "limit my use" directive (CA), or notice only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SensitiveModel {
    OptIn,
    LimitOnRequest,
    NoticeOnly,
}

impl SensitiveModel {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "opt_in" => Ok(Self::OptIn),
            "limit_on_request" => Ok(Self::LimitOnRequest),
            "notice_only" => Ok(Self::NoticeOnly),
            other => Err(PrivacyError::InvalidSensitiveModel(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptIn => "opt_in",
            Self::LimitOnRequest => "limit_on_request",
            Self::NoticeOnly => "notice_only",
        }
    }
}

/// A processing purpose. `Essential` can never be denied and is never
/// consentable; everything else resolves through the directive engine.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProcessingPurpose {
    Essential,
    Analytics,
    MarketingEmail,
    AdsPersonalisation,
    AdsMeasurement,
    SessionReplay,
    SaleOrShare,
    TargetedAdvertising,
    ProfilingSignificantEffects,
    SensitivePiUse,
}

impl ProcessingPurpose {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "essential" => Ok(Self::Essential),
            "analytics" => Ok(Self::Analytics),
            "marketing_email" => Ok(Self::MarketingEmail),
            "ads_personalisation" => Ok(Self::AdsPersonalisation),
            "ads_measurement" => Ok(Self::AdsMeasurement),
            "session_replay" => Ok(Self::SessionReplay),
            "sale_or_share" => Ok(Self::SaleOrShare),
            "targeted_advertising" => Ok(Self::TargetedAdvertising),
            "profiling_significant_effects" => Ok(Self::ProfilingSignificantEffects),
            "sensitive_pi_use" => Ok(Self::SensitivePiUse),
            other => Err(PrivacyError::InvalidPurpose(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Essential => "essential",
            Self::Analytics => "analytics",
            Self::MarketingEmail => "marketing_email",
            Self::AdsPersonalisation => "ads_personalisation",
            Self::AdsMeasurement => "ads_measurement",
            Self::SessionReplay => "session_replay",
            Self::SaleOrShare => "sale_or_share",
            Self::TargetedAdvertising => "targeted_advertising",
            Self::ProfilingSignificantEffects => "profiling_significant_effects",
            Self::SensitivePiUse => "sensitive_pi_use",
        }
    }

    /// Every purpose, in the order the directive snapshot renders them.
    pub const ALL: [Self; 10] = [
        Self::Essential,
        Self::Analytics,
        Self::MarketingEmail,
        Self::AdsPersonalisation,
        Self::AdsMeasurement,
        Self::SessionReplay,
        Self::SaleOrShare,
        Self::TargetedAdvertising,
        Self::ProfilingSignificantEffects,
        Self::SensitivePiUse,
    ];

    /// Purposes a universal opt-out mechanism (GPC/UOOM) always flips to
    /// denied, per the jurisdiction profiles' `honours_universal_opt_out`.
    pub const UNIVERSAL_OPT_OUT: [Self; 2] = [Self::SaleOrShare, Self::TargetedAdvertising];

    /// The non-essential set a consent/opt-out request may target.
    pub fn consentable(self) -> bool {
        self != Self::Essential
    }
}

/// The resolved answer for one purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveState {
    Allowed,
    Denied,
}

impl DirectiveState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "allowed" => Ok(Self::Allowed),
            "denied" => Ok(Self::Denied),
            other => Err(PrivacyError::InvalidDirectiveState(other.to_string())),
        }
    }
}

/// Where a directive's state came from. The jurisdiction profile supplies
/// the default; everything else is an explicit subject action or signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveSource {
    Consent,
    OptOut,
    Signal,
    Default,
    Guardian,
}

impl DirectiveSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Consent => "consent",
            Self::OptOut => "opt_out",
            Self::Signal => "signal",
            Self::Default => "default",
            Self::Guardian => "guardian",
        }
    }

    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "consent" => Ok(Self::Consent),
            "opt_out" => Ok(Self::OptOut),
            "signal" => Ok(Self::Signal),
            "default" => Ok(Self::Default),
            "guardian" => Ok(Self::Guardian),
            other => Err(PrivacyError::InvalidDirectiveSource(other.to_string())),
        }
    }
}

/// The sensitivity class of a mapped personal-data field. Drives the CA
/// "limit use" right, the non-CA opt-in for sensitive data, and the flagged
/// health-data and minors classes that trigger consent-first handling.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityClass {
    Standard,
    Sensitive,
    Special,
    FlaggedHealth,
    Minors,
}

impl SensitivityClass {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "standard" => Ok(Self::Standard),
            "sensitive" => Ok(Self::Sensitive),
            "special" => Ok(Self::Special),
            "flagged_health" => Ok(Self::FlaggedHealth),
            "minors" => Ok(Self::Minors),
            other => Err(PrivacyError::InvalidSensitivityClass(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Sensitive => "sensitive",
            Self::Special => "special",
            Self::FlaggedHealth => "flagged_health",
            Self::Minors => "minors",
        }
    }

    /// Whether processing this class requires a per-subject opt-in in
    /// consent-first jurisdictions (and a "limit use" right in CA).
    pub fn requires_sensitive_handling(self) -> bool {
        !matches!(self, Self::Standard)
    }
}

/// A legal basis for processing a data class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LegalBasis {
    Contract,
    LegalObligation,
    Consent,
    LegitimateInterest,
    NoticeOptOut,
    StatutoryRetention,
}

impl LegalBasis {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "contract" => Ok(Self::Contract),
            "legal_obligation" => Ok(Self::LegalObligation),
            "consent" => Ok(Self::Consent),
            "legitimate_interest" => Ok(Self::LegitimateInterest),
            "notice_opt_out" => Ok(Self::NoticeOptOut),
            "statutory_retention" => Ok(Self::StatutoryRetention),
            other => Err(PrivacyError::InvalidLegalBasis(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contract => "contract",
            Self::LegalObligation => "legal_obligation",
            Self::Consent => "consent",
            Self::LegitimateInterest => "legitimate_interest",
            Self::NoticeOptOut => "notice_opt_out",
            Self::StatutoryRetention => "statutory_retention",
        }
    }
}

/// What the retention sweep does when a class's period expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetentionAction {
    Delete,
    Anonymise,
    Archive,
}

impl RetentionAction {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "delete" => Ok(Self::Delete),
            "anonymise" => Ok(Self::Anonymise),
            "archive" => Ok(Self::Archive),
            other => Err(PrivacyError::InvalidRetentionAction(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Anonymise => "anonymise",
            Self::Archive => "archive",
        }
    }
}

/// A data class: the granularity at which the data map, retention rules and
/// breach classification operate. The registry below is the compile-time
/// data map; every provider names its classes from it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    IdentityProfile,
    Authentication,
    Membership,
    Contact,
    Billing,
    Invoice,
    /// A payment transaction record on a tenant's connected account
    /// (phase 09): identifiers, statuses and amounts. Statutorily
    /// retained — an erasure request must never delete a transaction.
    TransactionRecord,
    AuditLog,
    ConsentRecord,
    Preference,
    Communication,
    Jurisdiction,
    RequestRecord,
    /// Directive-gated conversion evidence, retained separately from the
    /// account record because it can contain hashed customer identifiers.
    AdvertisingConversionEvent,
    /// Daily tenant/platform/campaign advertising rollups.
    AdvertisingMetric,
    /// Who changed which campaign field, and when.
    AdvertisingCampaignChange,
    /// A tenant's advertising spend cap and its schedule.
    AdvertisingBudgetCap,
    /// A breach of one of those caps, and what was done about it.
    AdvertisingBudgetAlert,
}

impl DataClass {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "identity_profile" => Ok(Self::IdentityProfile),
            "authentication" => Ok(Self::Authentication),
            "membership" => Ok(Self::Membership),
            "contact" => Ok(Self::Contact),
            "billing" => Ok(Self::Billing),
            "invoice" => Ok(Self::Invoice),
            "transaction_record" => Ok(Self::TransactionRecord),
            "audit_log" => Ok(Self::AuditLog),
            "consent_record" => Ok(Self::ConsentRecord),
            "preference" => Ok(Self::Preference),
            "communication" => Ok(Self::Communication),
            "jurisdiction" => Ok(Self::Jurisdiction),
            "request_record" => Ok(Self::RequestRecord),
            "advertising_conversion_event" => Ok(Self::AdvertisingConversionEvent),
            "advertising_metric" => Ok(Self::AdvertisingMetric),
            "advertising_campaign_change" => Ok(Self::AdvertisingCampaignChange),
            "advertising_budget_cap" => Ok(Self::AdvertisingBudgetCap),
            "advertising_budget_alert" => Ok(Self::AdvertisingBudgetAlert),
            other => Err(PrivacyError::InvalidDataClass(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::IdentityProfile => "identity_profile",
            Self::Authentication => "authentication",
            Self::Membership => "membership",
            Self::Contact => "contact",
            Self::Billing => "billing",
            Self::Invoice => "invoice",
            Self::TransactionRecord => "transaction_record",
            Self::AuditLog => "audit_log",
            Self::ConsentRecord => "consent_record",
            Self::Preference => "preference",
            Self::Communication => "communication",
            Self::Jurisdiction => "jurisdiction",
            Self::RequestRecord => "request_record",
            Self::AdvertisingConversionEvent => "advertising_conversion_event",
            Self::AdvertisingMetric => "advertising_metric",
            Self::AdvertisingCampaignChange => "advertising_campaign_change",
            Self::AdvertisingBudgetCap => "advertising_budget_cap",
            Self::AdvertisingBudgetAlert => "advertising_budget_alert",
        }
    }
}

/// A DSR kind. `KnowCategories` is the CCPA "right to know" (categories
/// only, no contents).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DsrKind {
    Access,
    Export,
    Erasure,
    Rectification,
    Restriction,
    Objection,
    OptOutSaleOrShare,
    OptOutTargetedAdvertising,
    OptOutProfiling,
    LimitSensitiveUse,
    KnowCategories,
}

impl DsrKind {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "access" => Ok(Self::Access),
            "export" => Ok(Self::Export),
            "erasure" => Ok(Self::Erasure),
            "rectification" => Ok(Self::Rectification),
            "restriction" => Ok(Self::Restriction),
            "objection" => Ok(Self::Objection),
            "opt_out_sale_share" => Ok(Self::OptOutSaleOrShare),
            "opt_out_targeted_advertising" => Ok(Self::OptOutTargetedAdvertising),
            "opt_out_profiling" => Ok(Self::OptOutProfiling),
            "limit_sensitive_use" => Ok(Self::LimitSensitiveUse),
            "know_categories" => Ok(Self::KnowCategories),
            other => Err(PrivacyError::InvalidDsrKind(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Access => "access",
            Self::Export => "export",
            Self::Erasure => "erasure",
            Self::Rectification => "rectification",
            Self::Restriction => "restriction",
            Self::Objection => "objection",
            Self::OptOutSaleOrShare => "opt_out_sale_share",
            Self::OptOutTargetedAdvertising => "opt_out_targeted_advertising",
            Self::OptOutProfiling => "opt_out_profiling",
            Self::LimitSensitiveUse => "limit_sensitive_use",
            Self::KnowCategories => "know_categories",
        }
    }

    /// The purposes this kind flips to `Denied` immediately (opt-outs must
    /// not be gated on identity verification).
    pub fn denied_purposes(self) -> Vec<ProcessingPurpose> {
        match self {
            Self::OptOutSaleOrShare => vec![ProcessingPurpose::SaleOrShare],
            Self::OptOutTargetedAdvertising => vec![ProcessingPurpose::TargetedAdvertising],
            Self::OptOutProfiling => vec![ProcessingPurpose::ProfilingSignificantEffects],
            Self::LimitSensitiveUse => vec![ProcessingPurpose::SensitivePiUse],
            Self::Restriction => ProcessingPurpose::ALL
                .into_iter()
                .filter(|p| p.consentable())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Kinds that take effect on submission, without the verification gate.
    pub fn is_opt_out(self) -> bool {
        matches!(
            self,
            Self::OptOutSaleOrShare
                | Self::OptOutTargetedAdvertising
                | Self::OptOutProfiling
                | Self::LimitSensitiveUse
        )
    }

    /// Kinds that collect data from the providers.
    pub fn collects(self) -> bool {
        matches!(self, Self::Access | Self::Export | Self::KnowCategories)
    }

    /// Kinds that run erasure against the providers.
    pub fn erases(self) -> bool {
        matches!(self, Self::Erasure)
    }

    /// Kinds that apply rectification against the providers.
    pub fn rectifies(self) -> bool {
        matches!(self, Self::Rectification)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DsrStatus {
    AwaitingVerification,
    InProgress,
    PartiallyComplete,
    Completed,
    Rejected,
    OnHold,
    UnderAppeal,
}

impl DsrStatus {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "awaiting_verification" => Ok(Self::AwaitingVerification),
            "in_progress" => Ok(Self::InProgress),
            "partially_complete" => Ok(Self::PartiallyComplete),
            "completed" => Ok(Self::Completed),
            "rejected" => Ok(Self::Rejected),
            "on_hold" => Ok(Self::OnHold),
            "under_appeal" => Ok(Self::UnderAppeal),
            other => Err(PrivacyError::InvalidDsrStatus(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingVerification => "awaiting_verification",
            Self::InProgress => "in_progress",
            Self::PartiallyComplete => "partially_complete",
            Self::Completed => "completed",
            Self::Rejected => "rejected",
            Self::OnHold => "on_hold",
            Self::UnderAppeal => "under_appeal",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Rejected)
    }
}

/// Who submitted a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Requester {
    Subject,
    AuthorizedAgent,
    TenantOperator,
}

impl Requester {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "subject" => Ok(Self::Subject),
            "authorized_agent" => Ok(Self::AuthorizedAgent),
            "tenant_operator" => Ok(Self::TenantOperator),
            other => Err(PrivacyError::InvalidRequester(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::AuthorizedAgent => "authorized_agent",
            Self::TenantOperator => "tenant_operator",
        }
    }
}

/// The erasure mode a provider receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EraseMode {
    Full,
}

/// Which piece of evidence produced a jurisdiction resolution step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    DeclaredResidency,
    BillingCountry,
    ShippingCountry,
    TenantRegion,
    IpCountry,
    Fallback,
}

impl EvidenceKind {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "declared_residency" => Ok(Self::DeclaredResidency),
            "billing_country" => Ok(Self::BillingCountry),
            "shipping_country" => Ok(Self::ShippingCountry),
            "tenant_region" => Ok(Self::TenantRegion),
            "ip_country" => Ok(Self::IpCountry),
            "fallback" => Ok(Self::Fallback),
            other => Err(PrivacyError::InvalidRequester(format!(
                "invalid evidence kind: {other}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeclaredResidency => "declared_residency",
            Self::BillingCountry => "billing_country",
            Self::ShippingCountry => "shipping_country",
            Self::TenantRegion => "tenant_region",
            Self::IpCountry => "ip_country",
            Self::Fallback => "fallback",
        }
    }
}

/// One resolution step with its input and outcome; the full chain is stored
/// on the subject so a misresolution can be audited and reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JurisdictionEvidence {
    pub step: EvidenceKind,
    pub input: String,
    pub outcome: JurisdictionRef,
}

/// One collected personal-data entry.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DataEntry {
    pub data_class: DataClass,
    pub category: String,
    pub label: String,
    pub value: serde_json::Value,
}

/// A provider's export contribution for one subject.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct DataBundle {
    pub context: String,
    pub entries: Vec<DataEntry>,
}

/// Why something was retained despite an erasure request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetentionReason {
    LegalHold,
    StatutoryRetention,
    BackupWindow,
    Operational,
}

impl RetentionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegalHold => "legal_hold",
            Self::StatutoryRetention => "statutory_retention",
            Self::BackupWindow => "backup_window",
            Self::Operational => "operational",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RetainedItem {
    pub data_class: DataClass,
    pub reason: RetentionReason,
    pub detail: String,
}

/// A provider's erasure report: what was deleted/anonymised and — explicitly
/// — what was retained and why.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct EraseReport {
    pub context: String,
    pub erased: usize,
    pub anonymised: usize,
    pub retained: Vec<RetainedItem>,
}

/// One field correction (rectification).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Correction {
    pub context: String,
    pub field: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct RectifyPatch {
    pub corrections: Vec<Correction>,
}

/// One resolved answer per purpose, with the provenance the privacy centre
/// renders.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PurposeDirective {
    pub purpose: ProcessingPurpose,
    pub state: DirectiveState,
    pub source: DirectiveSource,
    pub jurisdiction: JurisdictionRef,
    pub notice_version: Option<String>,
    pub effective_at: DateTime<Utc>,
    pub superseded_at: Option<DateTime<Utc>>,
}

/// The full resolved state for one subject.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DirectiveSnapshot {
    pub subject: SubjectRef,
    pub jurisdiction: JurisdictionRef,
    pub directives: Vec<PurposeDirective>,
    pub minor_opt_in_age: Option<u8>,
    pub honours_universal_opt_out: bool,
}

/// How consent was recorded (drives proof requirements).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConsentMethod {
    Consent,
    OptOut,
    Withdrawal,
    GuardianConsent,
}

impl ConsentMethod {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "consent" => Ok(Self::Consent),
            "opt_out" => Ok(Self::OptOut),
            "withdrawal" => Ok(Self::Withdrawal),
            "guardian_consent" => Ok(Self::GuardianConsent),
            other => Err(PrivacyError::InvalidConsentMethod(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Consent => "consent",
            Self::OptOut => "opt_out",
            Self::Withdrawal => "withdrawal",
            Self::GuardianConsent => "guardian_consent",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignalSource {
    Ui,
    Api,
    Gpc,
    Uoom,
    Guardian,
    Import,
}

impl SignalSource {
    pub fn parse(raw: &str) -> Result<Self, PrivacyError> {
        match raw {
            "ui" => Ok(Self::Ui),
            "api" => Ok(Self::Api),
            "gpc" => Ok(Self::Gpc),
            "uoom" => Ok(Self::Uoom),
            "guardian" => Ok(Self::Guardian),
            "import" => Ok(Self::Import),
            other => Err(PrivacyError::InvalidSignalSource(other.to_string())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Api => "api",
            Self::Gpc => "gpc",
            Self::Uoom => "uoom",
            Self::Guardian => "guardian",
            Self::Import => "import",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `data_class` seeded into `privacy.retention_rules` must be a
    /// class this enum knows.
    ///
    /// A migration that adds a rule without adding the matching variant
    /// compiles fine and passes every unit test, then fails at runtime the
    /// first time anything reads the rules — which is how
    /// `advertising_budget_alert` reached `main` and broke retention sweeps
    /// and the public notice. Reading the migrations directly catches that at
    /// the point the rule is introduced, with no database.
    #[test]
    fn every_seeded_retention_rule_class_is_known() {
        let migrations =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        let mut checked = 0;
        for entry in std::fs::read_dir(&migrations).expect("migrations directory must exist") {
            let path = entry.expect("readable migration entry").path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            // `.down.sql` deletes rules rather than seeding them.
            if !name.ends_with(".sql") || name.ends_with(".down.sql") {
                continue;
            }
            let sql = std::fs::read_to_string(&path).expect("readable migration");
            for raw in seeded_retention_classes(&sql) {
                assert!(
                    DataClass::parse(&raw).is_ok(),
                    "{name} seeds retention rule for unknown data class {raw:?}"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "found no seeded retention rules — the scan stopped matching"
        );
    }

    /// Pull the quoted `data_class` values out of every
    /// `INSERT INTO privacy.retention_rules ... VALUES` statement.
    fn seeded_retention_classes(sql: &str) -> Vec<String> {
        let mut classes = Vec::new();
        for statement in sql.split("INSERT INTO privacy.retention_rules").skip(1) {
            let Some(values) = statement.split_once("VALUES") else {
                continue;
            };
            let body = values.1.split(';').next().unwrap_or_default();
            // Each row opens with `('<data_class>', ...`.
            for row in body.split('(').skip(1) {
                let trimmed = row.trim_start();
                if let Some(rest) = trimmed.strip_prefix('\'')
                    && let Some((class, _)) = rest.split_once('\'')
                {
                    classes.push(class.to_string());
                }
            }
        }
        classes
    }

    #[test]
    fn purpose_roundtrips() {
        for purpose in ProcessingPurpose::ALL {
            assert_eq!(ProcessingPurpose::parse(purpose.as_str()).unwrap(), purpose);
        }
    }

    #[test]
    fn purpose_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProcessingPurpose::SaleOrShare).unwrap(),
            "\"sale_or_share\""
        );
    }

    #[test]
    fn subject_ref_keys_are_stable() {
        let user = SubjectRef::user(UserId::try_from(123_456_789_i64).unwrap());
        assert_eq!(user.canonical_key(), "user:123456789");
        let other = SubjectRef::user(UserId::try_from(123_456_789_i64).unwrap());
        assert_eq!(user, other);
    }

    #[test]
    fn opt_out_kinds_take_effect_immediately() {
        assert!(DsrKind::OptOutSaleOrShare.is_opt_out());
        assert!(DsrKind::OptOutProfiling.is_opt_out());
        assert!(!DsrKind::Erasure.is_opt_out());
        assert_eq!(
            DsrKind::OptOutSaleOrShare.denied_purposes(),
            vec![ProcessingPurpose::SaleOrShare]
        );
    }

    #[test]
    fn essential_is_never_consentable() {
        assert!(!ProcessingPurpose::Essential.consentable());
        assert!(ProcessingPurpose::MarketingEmail.consentable());
    }
}
