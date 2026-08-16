//! The jurisdiction matrix test: the same subject journey, run against
//! every seeded profile, asserting the resolved default per purpose, the
//! due dates, the acknowledgement requirement and the appeal availability.
//!
//! Adding a profile row without extending the expectation table below fails
//! this test — a new US state law is a data change **plus an explicit
//! expectation**, never a silent assumption.

use sanvi_privacy::domain::{
    DirectiveState, JurisdictionRef, ProcessingPurpose, Regime, add_business_days, add_days,
    jurisdiction_default, seeded_profiles,
};

/// One expectation row per seeded profile. `allowed` lists the purposes
/// that default to allowed *before any subject action*; everything else
/// must default to denied.
struct ProfileExpectation {
    code: &'static str,
    regime: Regime,
    /// Default-allowed purposes (non-essential only; essential is always
    /// allowed in every jurisdiction).
    allowed: &'static [ProcessingPurpose],
    ack_days: Option<u16>,
    response_days: u16,
    extension_days: u16,
    opt_out_effective_business_days: Option<u16>,
    appeal_window_days: Option<u16>,
    appeal_response_days: Option<u16>,
    honours_universal_opt_out: bool,
    minor_opt_in_age: Option<u8>,
}

const EXPECTATIONS: &[ProfileExpectation] = &[
    ProfileExpectation {
        code: "eu",
        regime: Regime::Gdpr,
        allowed: &[],
        ack_days: None,
        response_days: 30,
        extension_days: 30,
        opt_out_effective_business_days: None,
        appeal_window_days: None,
        appeal_response_days: None,
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(16),
    },
    ProfileExpectation {
        code: "uk",
        regime: Regime::UkGdpr,
        allowed: &[],
        ack_days: None,
        response_days: 30,
        extension_days: 30,
        opt_out_effective_business_days: None,
        appeal_window_days: None,
        appeal_response_days: None,
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(16),
    },
    ProfileExpectation {
        code: "jp",
        regime: Regime::Appi,
        allowed: &[],
        ack_days: None,
        response_days: 14,
        extension_days: 0,
        opt_out_effective_business_days: None,
        appeal_window_days: None,
        appeal_response_days: None,
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(16),
    },
    ProfileExpectation {
        code: "us-ca",
        regime: Regime::UsState,
        allowed: &[
            ProcessingPurpose::Analytics,
            ProcessingPurpose::SaleOrShare,
            ProcessingPurpose::TargetedAdvertising,
            // CA's limit-on-request model: sensitive use allowed until
            // the "limit my sensitive PI" directive arrives.
            ProcessingPurpose::SensitivePiUse,
        ],
        ack_days: Some(10),
        response_days: 45,
        extension_days: 45,
        opt_out_effective_business_days: Some(15),
        appeal_window_days: None, // CA has no appeal right
        appeal_response_days: None,
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(16),
    },
    ProfileExpectation {
        code: "us-va",
        regime: Regime::UsState,
        allowed: &[
            ProcessingPurpose::Analytics,
            ProcessingPurpose::SaleOrShare,
            ProcessingPurpose::TargetedAdvertising,
        ],
        ack_days: Some(10),
        response_days: 45,
        extension_days: 45,
        opt_out_effective_business_days: Some(15),
        appeal_window_days: Some(30),
        appeal_response_days: Some(60),
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(13),
    },
    ProfileExpectation {
        code: "us-co",
        regime: Regime::UsState,
        allowed: &[
            ProcessingPurpose::Analytics,
            ProcessingPurpose::SaleOrShare,
            ProcessingPurpose::TargetedAdvertising,
        ],
        ack_days: Some(10),
        response_days: 45,
        extension_days: 45,
        opt_out_effective_business_days: Some(15),
        appeal_window_days: Some(45),
        appeal_response_days: Some(45),
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(13),
    },
    ProfileExpectation {
        code: "us-tx",
        regime: Regime::UsState,
        allowed: &[
            ProcessingPurpose::Analytics,
            ProcessingPurpose::SaleOrShare,
            ProcessingPurpose::TargetedAdvertising,
        ],
        ack_days: Some(10),
        response_days: 45,
        extension_days: 45,
        opt_out_effective_business_days: Some(15),
        appeal_window_days: Some(30),
        appeal_response_days: Some(60),
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(13),
    },
    ProfileExpectation {
        code: "us-ut",
        regime: Regime::UsState,
        allowed: &[
            ProcessingPurpose::Analytics,
            ProcessingPurpose::SaleOrShare,
            ProcessingPurpose::TargetedAdvertising,
        ],
        ack_days: Some(10),
        response_days: 45,
        extension_days: 45,
        opt_out_effective_business_days: Some(15),
        appeal_window_days: None, // UCPA: no appeal right
        appeal_response_days: None,
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(13),
    },
    ProfileExpectation {
        code: "us-ct",
        regime: Regime::UsState,
        allowed: &[
            ProcessingPurpose::Analytics,
            ProcessingPurpose::SaleOrShare,
            ProcessingPurpose::TargetedAdvertising,
        ],
        ack_days: Some(10),
        response_days: 45,
        extension_days: 45,
        opt_out_effective_business_days: Some(15),
        appeal_window_days: Some(45),
        appeal_response_days: Some(45),
        honours_universal_opt_out: true,
        minor_opt_in_age: Some(13),
    },
];

#[test]
fn every_seeded_profile_has_an_expectation_row() {
    let profiles = seeded_profiles();
    for profile in &profiles {
        assert!(
            EXPECTATIONS
                .iter()
                .any(|expectation| expectation.code == profile.code.as_str()),
            "profile `{}` is seeded but has no matrix expectation — add one (a new jurisdiction is a data change plus a test)",
            profile.code.as_str()
        );
    }
    assert_eq!(
        EXPECTATIONS.len(),
        profiles.len(),
        "an expectation row exists for a profile that is not seeded"
    );
}

#[test]
fn the_same_journey_resolves_per_profile() {
    for expectation in EXPECTATIONS {
        let profile = seeded_profiles()
            .into_iter()
            .find(|p| p.code.as_str() == expectation.code)
            .unwrap_or_else(|| panic!("missing seeded profile {}", expectation.code));

        // Structural fields.
        assert_eq!(profile.regime, expectation.regime, "{}", expectation.code);
        assert_eq!(
            profile.ack_days, expectation.ack_days,
            "{}",
            expectation.code
        );
        assert_eq!(
            profile.response_days, expectation.response_days,
            "{}",
            expectation.code
        );
        assert_eq!(
            profile.extension_days, expectation.extension_days,
            "{}",
            expectation.code
        );
        assert_eq!(
            profile.opt_out_effective_business_days, expectation.opt_out_effective_business_days,
            "{}",
            expectation.code
        );
        assert_eq!(
            profile.appeal_window_days, expectation.appeal_window_days,
            "{}",
            expectation.code
        );
        assert_eq!(
            profile.appeal_response_days, expectation.appeal_response_days,
            "{}",
            expectation.code
        );
        assert_eq!(
            profile.honours_universal_opt_out, expectation.honours_universal_opt_out,
            "{}",
            expectation.code
        );
        assert_eq!(
            profile.minor_opt_in_age, expectation.minor_opt_in_age,
            "{}",
            expectation.code
        );

        // Defaults per purpose.
        for purpose in ProcessingPurpose::ALL {
            if purpose == ProcessingPurpose::Essential {
                continue;
            }
            let (state, _source) = jurisdiction_default(&profile, purpose);
            let expected = if expectation.allowed.contains(&purpose) {
                DirectiveState::Allowed
            } else {
                DirectiveState::Denied
            };
            assert_eq!(
                state,
                expected,
                "{}: default for {} must be {}",
                expectation.code,
                purpose.as_str(),
                if expected == DirectiveState::Allowed {
                    "allowed"
                } else {
                    "denied"
                }
            );
        }

        // Essential is always allowed.
        let (essential_state, _) = jurisdiction_default(&profile, ProcessingPurpose::Essential);
        assert_eq!(
            essential_state,
            DirectiveState::Allowed,
            "{}",
            expectation.code
        );
    }
}

#[test]
fn the_us_ack_and_due_clocks_come_from_the_profile() {
    // A CA subject submits an erasure request: the ack window is 10
    // business days and the response due date is 45 calendar days.
    let ca = seeded_profiles()
        .into_iter()
        .find(|p| p.code.as_str() == "us-ca")
        .expect("us-ca seeded");
    let received = chrono::Utc::now();
    let ack_due = ca.ack_days.map(|days| add_business_days(received, days));
    let due_at = add_days(received, ca.response_days);
    assert!(ack_due.is_some());
    assert!(
        ack_due.unwrap() < due_at,
        "ack must precede the response due date"
    );

    // An EU subject has no ack window (GDPR has no acknowledgement
    // deadline) and a 30-day response window.
    let eu = seeded_profiles()
        .into_iter()
        .find(|p| p.code.as_str() == "eu")
        .expect("eu seeded");
    assert_eq!(eu.ack_days, None);
    let eu_due = add_days(received, eu.response_days);
    assert!(
        eu_due < due_at,
        "GDPR's 30-day window is shorter than CCPA's 45"
    );
}

#[test]
fn opt_out_semantics_are_identical_across_jurisdictions() {
    // The opt-out code path is jurisdiction-independent: an exercised
    // opt-out flips the purpose to denied in every profile, immediately,
    // with no verification gate. This test pins the *code path*, not the
    // jurisdiction.
    for profile in seeded_profiles() {
        let directive = sanvi_privacy::domain::resolve_purpose(
            &profile,
            ProcessingPurpose::SaleOrShare,
            Some(&sanvi_privacy::domain::DirectiveInput {
                purpose: ProcessingPurpose::SaleOrShare,
                state: DirectiveState::Denied,
                source: sanvi_privacy::domain::DirectiveSource::OptOut,
                notice_version: None,
            }),
            None,
            false,
            None,
            chrono::Utc::now(),
        );
        assert_eq!(
            directive.state,
            DirectiveState::Denied,
            "{}",
            profile.code.as_str()
        );
        assert_eq!(
            directive.source,
            sanvi_privacy::domain::DirectiveSource::OptOut,
            "{}",
            profile.code.as_str()
        );
    }
}

#[test]
fn gpc_is_honoured_in_every_profile() {
    for profile in seeded_profiles() {
        let directive = sanvi_privacy::domain::resolve_purpose(
            &profile,
            ProcessingPurpose::TargetedAdvertising,
            None,
            Some(&sanvi_privacy::domain::DirectiveInput {
                purpose: ProcessingPurpose::TargetedAdvertising,
                state: DirectiveState::Denied,
                source: sanvi_privacy::domain::DirectiveSource::Signal,
                notice_version: None,
            }),
            false,
            None,
            chrono::Utc::now(),
        );
        assert_eq!(
            directive.state,
            DirectiveState::Denied,
            "{}",
            profile.code.as_str()
        );
        assert_eq!(
            directive.source,
            sanvi_privacy::domain::DirectiveSource::Signal,
            "{}",
            profile.code.as_str()
        );
    }
}

#[test]
fn minors_cannot_sell_or_share_in_any_jurisdiction() {
    // Under-16 US subjects require opt-in before sale/share; under-13 is
    // blocked outright. The guardian gate is jurisdiction-independent: the
    // profile only supplies the threshold age.
    for profile in seeded_profiles() {
        let directive = sanvi_privacy::domain::resolve_purpose(
            &profile,
            ProcessingPurpose::SaleOrShare,
            None,
            None,
            true,
            None,
            chrono::Utc::now(),
        );
        assert_eq!(
            directive.state,
            DirectiveState::Denied,
            "{} must deny sale/share for known minors without guardian consent",
            profile.code.as_str()
        );
    }
}

#[test]
fn breach_rules_seed_has_the_three_regime_shapes() {
    // GDPR/UK: hour-level authority deadline. APPI: day-level authority
    // deadline. US states: threshold-gated, day-level, encryption
    // safe-harbour varies.
    let profiles = seeded_profiles();
    let eu = profiles.iter().find(|p| p.code.as_str() == "eu").unwrap();
    assert_eq!(eu.breach_rules.authority_due_hours, Some(72));
    assert!(eu.breach_rules.encryption_safe_harbour);

    let jp = profiles.iter().find(|p| p.code.as_str() == "jp").unwrap();
    assert!(jp.breach_rules.authority_due_days.is_some());

    let ca = profiles
        .iter()
        .find(|p| p.code.as_str() == "us-ca")
        .unwrap();
    assert_eq!(ca.breach_rules.authority_threshold, Some(500));
    assert!(ca.breach_rules.encryption_safe_harbour);

    let co = profiles
        .iter()
        .find(|p| p.code.as_str() == "us-co")
        .unwrap();
    assert_eq!(co.breach_rules.consumer_due_days, Some(30));
    assert!(!co.breach_rules.encryption_safe_harbour);
}

#[test]
fn jurisdiction_codes_parse_and_reject() {
    assert!(JurisdictionRef::parse("us-ca").is_ok());
    assert!(JurisdictionRef::parse("EU").unwrap().as_str() == "eu");
    assert!(JurisdictionRef::parse("bad code!").is_err());
}
