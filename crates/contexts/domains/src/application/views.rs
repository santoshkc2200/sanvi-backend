use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::{
    CustomDomain, CustomDomainId, DnsRecord, DomainFailure, DomainOrder, DomainOrderId, Hostname,
    VerificationChallenge,
};

/// The challenge as shown to the tenant (the token itself is the secret
/// they must publish; the value is pre-rendered for copy-paste).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ChallengeView {
    pub token: String,
    pub challenge_type: String,
    pub txt_name: String,
    pub txt_value: String,
    pub expires_at: DateTime<Utc>,
}

impl ChallengeView {
    pub fn of(challenge: &VerificationChallenge, hostname: &Hostname) -> Self {
        Self {
            token: challenge.token.clone(),
            challenge_type: challenge.challenge_type.clone(),
            txt_name: challenge.txt_name(hostname),
            txt_value: challenge.txt_value(),
            expires_at: challenge.expires_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CustomDomainView {
    #[schema(value_type = String)]
    pub id: CustomDomainId,
    pub hostname: String,
    pub kind: String,
    pub role: String,
    pub status: String,
    pub detected_registrar: Option<String>,
    pub verified_at: Option<DateTime<Utc>>,
    pub challenge: Option<ChallengeView>,
    pub failure: Option<DomainFailure>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CustomDomainView {
    pub fn of(domain: &CustomDomain) -> Self {
        Self {
            id: domain.id,
            hostname: domain.hostname.as_str().to_string(),
            kind: domain.kind.as_str().to_string(),
            role: domain.role.as_str().to_string(),
            status: domain.status.as_str().to_string(),
            detected_registrar: domain.detected_registrar.clone(),
            verified_at: domain.verified_at,
            challenge: domain
                .challenge
                .as_ref()
                .map(|challenge| ChallengeView::of(challenge, &domain.hostname)),
            failure: domain.failure.clone(),
            created_at: domain.created_at,
            updated_at: domain.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OrderView {
    #[schema(value_type = String)]
    pub id: DomainOrderId,
    pub hostname: String,
    pub term_years: u8,
    pub price_minor: i64,
    pub currency: String,
    pub status: String,
    pub auto_renew: bool,
    pub whois_privacy: bool,
    pub registered_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl OrderView {
    pub fn of(order: &DomainOrder) -> Self {
        Self {
            id: order.id,
            hostname: order.hostname.as_str().to_string(),
            term_years: order.term_years,
            price_minor: order.price.minor,
            currency: order.price.currency.to_string(),
            status: order.status.as_str().to_string(),
            auto_renew: order.auto_renew,
            whois_privacy: order.whois_privacy,
            registered_at: order.registered_at,
            expires_at: order.expires_at,
            created_at: order.created_at,
        }
    }
}

/// One instruction record with a localized explanation line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InstructionRecord {
    pub name: String,
    pub record_type: String,
    pub value: String,
    pub ttl: u32,
    pub explanation: String,
}

impl InstructionRecord {
    pub fn of(record: &DnsRecord, explanation: String) -> Self {
        Self {
            name: record.name.clone(),
            record_type: record.record_type.as_str().to_string(),
            value: record.value.clone(),
            ttl: record.ttl,
            explanation,
        }
    }
}

/// The registrar-specific walkthrough (localized; phase 06).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct GuideView {
    pub id: String,
    pub title: String,
    pub steps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SearchView {
    pub query: String,
    pub registrar_id: String,
    pub results: Vec<crate::domain::DomainQuote>,
}
