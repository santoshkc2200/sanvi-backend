//! Jurisdiction profiles: the configuration that every deadline, default
//! and appeal right derives from. Profiles are DATA — the authoritative
//! store is `privacy.jurisdiction_profiles` (seeded by migration, reviewed
//! by counsel); the `SEEDED_PROFILES` constant below mirrors the seed so the
//! matrix test can assert expectations without a database, and a dedicated
//! CI test proves the two never drift apart.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::types::{
    ConsentModel, EvidenceKind, JurisdictionEvidence, JurisdictionRef, Regime, SensitiveModel,
};

/// Breach-notification rules per jurisdiction. jsonb in the database so a
/// new law is a data change; typed here so the calculator cannot misparse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
pub struct BreachRuleSet {
    /// Deadline to notify the supervisory authority, in hours.
    pub authority_due_hours: Option<u16>,
    /// Deadline to notify the authority, in days (states without an
    /// hour-level rule).
    pub authority_due_days: Option<u16>,
    /// Minimum number of affected subjects before the authority must be
    /// notified at all (several US states).
    pub authority_threshold: Option<u32>,
    /// Deadline to notify consumers, in days.
    pub consumer_due_days: Option<u16>,
    /// Minimum affected count before consumers must be notified.
    pub consumer_threshold: Option<u32>,
    /// Whether encryption-at-rest of the compromised data suppresses the
    /// obligations this rule set marks as encryption-exempt (safe harbour).
    pub encryption_safe_harbour: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
pub struct AuthorityContact {
    pub name: String,
    pub complaint_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct JurisdictionProfile {
    pub code: JurisdictionRef,
    pub regime: Regime,
    pub consent_model: ConsentModel,
    pub sensitive_model: SensitiveModel,
    pub ack_days: Option<u16>,
    pub response_days: u16,
    pub extension_days: u16,
    pub opt_out_effective_business_days: Option<u16>,
    pub appeal_window_days: Option<u16>,
    pub appeal_response_days: Option<u16>,
    pub honours_universal_opt_out: bool,
    pub minor_opt_in_age: Option<u8>,
    pub risk_assessment_required: bool,
    pub breach_rules: BreachRuleSet,
    pub authority: AuthorityContact,
    pub effective_from: NaiveDate,
    pub source_note: Option<String>,
}

/// The profile code every unresolved subject falls back to: most protective.
pub const MOST_PROTECTIVE_CODE: &str = "eu";

/// The profile every unresolved subject falls back to: most protective.
pub fn most_protective() -> JurisdictionRef {
    JurisdictionRef::parse(MOST_PROTECTIVE_CODE).expect("hardcoded code parses")
}

/// The seeded profiles (mirrors `migrations/20260819000000_privacy_compliance.sql`).
/// Adding a profile here without extending the matrix test fails CI; the
/// migration-drift test fails if this list and the seed diverge.
pub fn seeded_profiles() -> Vec<JurisdictionProfile> {
    let eu = JurisdictionRef::from_static("eu");
    let uk = JurisdictionRef::from_static("uk");
    let jp = JurisdictionRef::from_static("jp");
    let us = |code: &'static str| JurisdictionRef::from_static(code);

    vec![
        JurisdictionProfile {
            code: eu.clone(),
            regime: Regime::Gdpr,
            consent_model: ConsentModel::OptIn,
            sensitive_model: SensitiveModel::OptIn,
            ack_days: None,
            response_days: 30,
            extension_days: 30,
            opt_out_effective_business_days: None,
            appeal_window_days: None,
            appeal_response_days: None,
            honours_universal_opt_out: true,
            minor_opt_in_age: Some(16),
            risk_assessment_required: true,
            breach_rules: BreachRuleSet {
                authority_due_hours: Some(72),
                authority_due_days: None,
                authority_threshold: Some(0),
                consumer_due_days: None,
                consumer_threshold: None,
                encryption_safe_harbour: true,
            },
            authority: AuthorityContact {
                name: "National supervisory authority (via EDPB)".to_string(),
                complaint_url: "https://edpb.europa.eu/".to_string(),
            },
            effective_from: NaiveDate::from_ymd_opt(2026, 8, 1).expect("valid date"),
            source_note: Some("GDPR Art. 12/15-22: 1 month extendable +2; Art. 33: 72h to the SA; Art. 35 DPIA. Seeded operating defaults — counsel review required.".to_string()),
        },
        JurisdictionProfile {
            code: uk.clone(),
            regime: Regime::UkGdpr,
            consent_model: ConsentModel::OptIn,
            sensitive_model: SensitiveModel::OptIn,
            ack_days: None,
            response_days: 30,
            extension_days: 30,
            opt_out_effective_business_days: None,
            appeal_window_days: None,
            appeal_response_days: None,
            honours_universal_opt_out: true,
            minor_opt_in_age: Some(16),
            risk_assessment_required: true,
            breach_rules: BreachRuleSet {
                authority_due_hours: Some(72),
                authority_due_days: None,
                authority_threshold: Some(0),
                consumer_due_days: None,
                consumer_threshold: None,
                encryption_safe_harbour: true,
            },
            authority: AuthorityContact {
                name: "Information Commissioner's Office (ICO)".to_string(),
                complaint_url: "https://ico.org.uk/make-a-complaint/".to_string(),
            },
            effective_from: NaiveDate::from_ymd_opt(2026, 8, 1).expect("valid date"),
            source_note: Some("UK GDPR + DPA 2018; ICO as supervisory authority; UK IDTA/Addendum for transfers. Seeded operating defaults — counsel review required.".to_string()),
        },
        JurisdictionProfile {
            code: jp.clone(),
            regime: Regime::Appi,
            consent_model: ConsentModel::OptIn,
            sensitive_model: SensitiveModel::OptIn,
            ack_days: None,
            response_days: 14,
            extension_days: 0,
            opt_out_effective_business_days: None,
            appeal_window_days: None,
            appeal_response_days: None,
            honours_universal_opt_out: true,
            minor_opt_in_age: Some(16),
            risk_assessment_required: false,
            breach_rules: BreachRuleSet {
                authority_due_hours: None,
                authority_due_days: Some(5),
                authority_threshold: Some(0),
                consumer_due_days: None,
                consumer_threshold: None,
                encryption_safe_harbour: false,
            },
            authority: AuthorityContact {
                name: "Personal Information Protection Commission (PPC)".to_string(),
                complaint_url: "https://www.ppc.go.jp/".to_string(),
            },
            effective_from: NaiveDate::from_ymd_opt(2026, 8, 1).expect("valid date"),
            source_note: Some("APPI: response \"without delay\" (internal SLA 14 days); breach report to the PPC \"promptly\"; no DPIA. Seeded operating defaults — counsel review required.".to_string()),
        },
        us_profile(
            us("us-ca"),
            SensitiveModel::LimitOnRequest,
            16,
            true,
            BreachRuleSet {
                authority_due_hours: None,
                authority_due_days: None,
                authority_threshold: Some(500),
                consumer_due_days: None,
                consumer_threshold: Some(0),
                encryption_safe_harbour: true,
            },
            ("California Privacy Protection Agency (CPPA)", "https://cppa.ca.gov/"),
            "CCPA/CPRA: ack 10 business days, response 45 (+45); opt-out effective 15 business days; no appeal right; risk assessments; encryption safe harbour. Seeded operating defaults — counsel review required.",
        ),
        us_profile(
            us("us-va"),
            SensitiveModel::OptIn,
            13,
            true,
            BreachRuleSet {
                authority_due_hours: None,
                authority_due_days: None,
                authority_threshold: Some(500),
                consumer_due_days: None,
                consumer_threshold: Some(0),
                encryption_safe_harbour: false,
            },
            ("Office of the Attorney General of Virginia", "https://www.oag.state.va.us/consumer-protection/"),
            "VCDPA: appeal decided within 60 days; data protection assessments; sensitive processing is opt-in. Seeded operating defaults — counsel review required.",
        ),
        us_profile(
            us("us-co"),
            SensitiveModel::OptIn,
            13,
            true,
            BreachRuleSet {
                authority_due_hours: None,
                authority_due_days: None,
                authority_threshold: Some(500),
                consumer_due_days: Some(30),
                consumer_threshold: Some(0),
                encryption_safe_harbour: false,
            },
            ("Colorado Attorney General", "https://coag.gov/office-sections/consumer-protection/data-privacy/"),
            "CPA: appeal within 45 days; data protection assessments; sensitive processing is opt-in; consumer breach notice 30 days. Seeded operating defaults — counsel review required.",
        ),
        us_profile(
            us("us-tx"),
            SensitiveModel::OptIn,
            13,
            true,
            BreachRuleSet {
                authority_due_hours: None,
                authority_due_days: None,
                authority_threshold: Some(250),
                consumer_due_days: None,
                consumer_threshold: Some(0),
                encryption_safe_harbour: false,
            },
            ("Texas Attorney General", "https://www.texasattorneygeneral.gov/"),
            "TDPSA: appeal decided within 60 days; data protection assessments; sensitive processing is opt-in. Seeded operating defaults — counsel review required.",
        ),
        us_profile(
            us("us-ut"),
            SensitiveModel::OptIn,
            13,
            false,
            BreachRuleSet {
                authority_due_hours: None,
                authority_due_days: None,
                authority_threshold: Some(500),
                consumer_due_days: None,
                consumer_threshold: Some(0),
                encryption_safe_harbour: false,
            },
            ("Utah Attorney General", "https://attorneygeneral.utah.gov/"),
            "UCPA: no appeal right, no DPIA; sensitive processing is opt-in. Seeded operating defaults — counsel review required.",
        ),
        us_profile(
            us("us-ct"),
            SensitiveModel::OptIn,
            13,
            true,
            BreachRuleSet {
                authority_due_hours: None,
                authority_due_days: None,
                authority_threshold: Some(500),
                consumer_due_days: None,
                consumer_threshold: Some(0),
                encryption_safe_harbour: false,
            },
            ("Connecticut Attorney General", "https://portal.ct.gov/ag"),
            "CTDPA: appeal within 45 days; data protection assessments; sensitive processing is opt-in. Seeded operating defaults — counsel review required.",
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn us_profile(
    code: JurisdictionRef,
    sensitive_model: SensitiveModel,
    minor_opt_in_age: u8,
    risk_assessment_required: bool,
    breach_rules: BreachRuleSet,
    authority: (&'static str, &'static str),
    source_note: &'static str,
) -> JurisdictionProfile {
    let appeal_window = match code.as_str() {
        "us-ut" | "us-ca" => None, // UCPA and CCPA/CPRA provide no appeal right
        "us-va" | "us-tx" => Some(30u16),
        "us-co" | "us-ct" => Some(45u16),
        _ => Some(45u16),
    };
    let appeal_response = match code.as_str() {
        "us-va" | "us-tx" => 60u16, // VCDPA/TDPSA: appeal decided within 60 days
        "us-co" | "us-ct" => 45u16, // CPA/CTDPA: within 45 days
        _ => 45u16,
    };
    JurisdictionProfile {
        code,
        regime: Regime::UsState,
        consent_model: ConsentModel::NoticeAndOptOut,
        sensitive_model,
        ack_days: Some(10),
        response_days: 45,
        extension_days: 45,
        opt_out_effective_business_days: Some(15),
        appeal_window_days: appeal_window,
        appeal_response_days: appeal_window.map(|_| appeal_response),
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(minor_opt_in_age),
        risk_assessment_required,
        breach_rules,
        authority: AuthorityContact {
            name: authority.0.to_string(),
            complaint_url: authority.1.to_string(),
        },
        effective_from: NaiveDate::from_ymd_opt(2026, 8, 1).expect("valid date"),
        source_note: Some(source_note.to_string()),
    }
}

/// Look up a seeded profile by code (the compile-time mirror of the DB
/// seed; the runtime resolver reads the database and falls back to the most
/// protective profile when a code has no row).
pub fn seeded_profile(code: &JurisdictionRef) -> Option<JurisdictionProfile> {
    seeded_profiles().into_iter().find(|p| &p.code == code)
}

/// The most protective seeded profile (the fallback).
pub fn most_protective_profile() -> JurisdictionProfile {
    seeded_profile(&most_protective()).expect("eu profile is seeded")
}

/// EEA member + EEA-adjacent country codes resolve to `eu`.
const EEA_COUNTRY_CODES: [&str; 30] = [
    "AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE", "IT", "LV",
    "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE", "IS", "LI", "NO",
];

/// US states with a seeded profile.
fn us_state_to_jurisdiction(state: &str) -> Option<JurisdictionRef> {
    let code = match state {
        "CA" => "us-ca",
        "VA" => "us-va",
        "CO" => "us-co",
        "TX" => "us-tx",
        "UT" => "us-ut",
        "CT" => "us-ct",
        _ => return None,
    };
    Some(JurisdictionRef::from_static(code))
}

/// Map a country (ISO 3166-1 alpha-2, `US-XX` for states, `EU-…` region
/// codes) to a jurisdiction profile, if one exists.
pub fn country_to_jurisdiction(cc: &str) -> Option<JurisdictionRef> {
    let cc = cc.trim().to_ascii_uppercase();
    if EEA_COUNTRY_CODES.contains(&cc.as_str()) {
        return Some(JurisdictionRef::from_static("eu"));
    }
    match cc.as_str() {
        "GB" => Some(JurisdictionRef::from_static("uk")),
        "JP" => Some(JurisdictionRef::from_static("jp")),
        _ => {
            if let Some(state) = cc.strip_prefix("US-") {
                us_state_to_jurisdiction(state)
            } else {
                None
            }
        }
    }
}

/// The resolution order (most trusted first); the caller supplies one entry
/// per available piece of evidence in this order and this function picks the
/// first that maps to a profile. Unresolved ⇒ most protective.
pub const RESOLUTION_ORDER: [EvidenceKind; 5] = [
    EvidenceKind::DeclaredResidency,
    EvidenceKind::BillingCountry,
    EvidenceKind::ShippingCountry,
    EvidenceKind::TenantRegion,
    EvidenceKind::IpCountry,
];

/// Resolve a jurisdiction from an ordered evidence chain, recording every
/// step. The final entry is always the outcome (or the most-protective
/// fallback).
pub fn resolve(
    evidence: &[(EvidenceKind, String)],
) -> (JurisdictionRef, Vec<JurisdictionEvidence>) {
    let mut chain = Vec::new();
    for (kind, input) in evidence {
        if let Some(code) = country_to_jurisdiction(input) {
            chain.push(JurisdictionEvidence {
                step: *kind,
                input: input.clone(),
                outcome: code.clone(),
            });
            return (code, chain);
        }
        chain.push(JurisdictionEvidence {
            step: *kind,
            input: input.clone(),
            outcome: most_protective(),
        });
    }
    chain.push(JurisdictionEvidence {
        step: EvidenceKind::Fallback,
        input: "no resolvable evidence".to_string(),
        outcome: most_protective(),
    });
    (most_protective(), chain)
}

/// Advance a timestamp by `days` business days (Mon–Fri). Used for the
/// CCPA-style acknowledgement and opt-out-effectiveness windows.
pub fn add_business_days(start: DateTime<Utc>, days: u16) -> DateTime<Utc> {
    let mut result = start;
    let mut remaining = days;
    while remaining > 0 {
        result += Duration::days(1);
        if !matches!(result.weekday(), Weekday::Sat | Weekday::Sun) {
            remaining -= 1;
        }
    }
    result
}

/// Advance a timestamp by calendar days.
pub fn add_days(start: DateTime<Utc>, days: u16) -> DateTime<Utc> {
    start + Duration::days(i64::from(days))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_mirror_the_migration() {
        // The authoritative drift check lives in tests/privacy_integration.rs
        // (parses the migration SQL); here we assert internal consistency.
        let profiles = seeded_profiles();
        let codes: Vec<&str> = profiles.iter().map(|p| p.code.as_str()).collect();
        for expected in [
            "eu", "uk", "jp", "us-ca", "us-va", "us-co", "us-tx", "us-ut", "us-ct",
        ] {
            assert!(
                codes.contains(&expected),
                "missing seeded profile {expected}"
            );
        }
        assert_eq!(codes.len(), 9);
    }

    #[test]
    fn country_codes_resolve() {
        assert_eq!(
            country_to_jurisdiction("de"),
            Some(JurisdictionRef::from_static("eu"))
        );
        assert_eq!(
            country_to_jurisdiction("GB"),
            Some(JurisdictionRef::from_static("uk"))
        );
        assert_eq!(
            country_to_jurisdiction("jp"),
            Some(JurisdictionRef::from_static("jp"))
        );
        assert_eq!(
            country_to_jurisdiction("US-CA"),
            Some(JurisdictionRef::from_static("us-ca"))
        );
        assert_eq!(
            country_to_jurisdiction("US-VA"),
            Some(JurisdictionRef::from_static("us-va"))
        );
        assert_eq!(country_to_jurisdiction("XX"), None);
    }

    #[test]
    fn resolution_prefers_declared_residency() {
        let evidence = vec![
            (EvidenceKind::DeclaredResidency, "DE".to_string()),
            (EvidenceKind::IpCountry, "US-CA".to_string()),
        ];
        let (code, chain) = resolve(&evidence);
        assert_eq!(code.as_str(), "eu");
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn resolution_falls_back_to_most_protective() {
        let (code, chain) = resolve(&[(EvidenceKind::IpCountry, "US-NY".to_string())]);
        assert_eq!(code, most_protective());
        // Unknown state: recorded as most-protective at that step, and the
        // final fallback entry is appended.
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.last().unwrap().step, EvidenceKind::Fallback);
    }

    #[test]
    fn business_days_skip_weekends() {
        // 2026-08-14 is a Friday; +1 business day is Monday 2026-08-17.
        let friday = DateTime::parse_from_rfc3339("2026-08-14T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let monday = add_business_days(friday, 1);
        assert_eq!(monday.date_naive().to_string(), "2026-08-17");
        // 5 business days from Friday = Friday next week.
        let next_friday = add_business_days(friday, 5);
        assert_eq!(next_friday.date_naive().to_string(), "2026-08-21");
    }
}
