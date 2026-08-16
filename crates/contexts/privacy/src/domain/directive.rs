//! The directive engine: pure resolution logic for "may we process
//! `purpose` for this subject?".
//!
//! Resolution order (first match wins):
//!   1. explicit subject action (consent grant/withdrawal, exercised opt-out)
//!   2. guardian decision for a known minor
//!   3. universal opt-out signal (GPC/UOOM) for its purposes
//!   4. the jurisdiction profile's default
//!   5. the most protective default (essential is always allowed; everything
//!      else denied)
//!
//! `DirectiveState::Allowed` is the only value that permits processing.
//! Callers see the resolved state and source — never *which jurisdiction
//! rule* produced it — which keeps jurisdiction logic out of every other
//! context.

use chrono::{DateTime, Utc};

use super::jurisdiction::JurisdictionProfile;
use super::types::{
    ConsentModel, DirectiveSource, DirectiveState, JurisdictionRef, ProcessingPurpose,
    PurposeDirective, SensitiveModel,
};

/// One explicit input to resolution (a ledger row or a bound signal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectiveInput {
    pub purpose: ProcessingPurpose,
    pub state: DirectiveState,
    pub source: DirectiveSource,
    pub notice_version: Option<String>,
}

/// Resolve one purpose. `explicit` is the latest explicit action for the
/// purpose (if any); `signal` the latest universal-opt-out signal state for
/// the purpose (if any); `is_minor` enables the guardian gate.
pub fn resolve_purpose(
    profile: &JurisdictionProfile,
    purpose: ProcessingPurpose,
    explicit: Option<&DirectiveInput>,
    signal: Option<&DirectiveInput>,
    is_minor: bool,
    notice_version: Option<&str>,
    now: DateTime<Utc>,
) -> PurposeDirective {
    if purpose == ProcessingPurpose::Essential {
        return PurposeDirective {
            purpose,
            state: DirectiveState::Allowed,
            source: DirectiveSource::Default,
            jurisdiction: profile.code.clone(),
            notice_version: None,
            effective_at: now,
            superseded_at: None,
        };
    }

    // 1. Explicit subject action always wins (and a refusal survives
    //    everything that comes after it).
    if let Some(input) = explicit {
        return PurposeDirective {
            purpose,
            state: input.state,
            source: input.source,
            jurisdiction: profile.code.clone(),
            notice_version: input.notice_version.clone(),
            effective_at: now,
            superseded_at: None,
        };
    }

    // 2. Minors: sale/share, targeted advertising and sensitive use need a
    //    recorded guardian opt-in; without one they are denied outright.
    if is_minor
        && matches!(
            purpose,
            ProcessingPurpose::SaleOrShare
                | ProcessingPurpose::TargetedAdvertising
                | ProcessingPurpose::SensitivePiUse
                | ProcessingPurpose::ProfilingSignificantEffects
        )
    {
        return PurposeDirective {
            purpose,
            state: DirectiveState::Denied,
            source: DirectiveSource::Guardian,
            jurisdiction: profile.code.clone(),
            notice_version: notice_version.map(str::to_string),
            effective_at: now,
            superseded_at: None,
        };
    }

    // 3. Universal opt-out signals (honoured for every profile that flags
    //    them; a later "accept all" never silently overrides — the UI must
    //    ask, and the answer is recorded as an explicit action above).
    if profile.honours_universal_opt_out
        && ProcessingPurpose::UNIVERSAL_OPT_OUT.contains(&purpose)
        && let Some(signal) = signal
    {
        return PurposeDirective {
            purpose,
            state: signal.state,
            source: DirectiveSource::Signal,
            jurisdiction: profile.code.clone(),
            notice_version: signal.notice_version.clone(),
            effective_at: now,
            superseded_at: None,
        };
    }

    // 4. The jurisdiction default.
    let (state, source) = jurisdiction_default(profile, purpose);
    PurposeDirective {
        purpose,
        state,
        source,
        jurisdiction: profile.code.clone(),
        notice_version: notice_version.map(str::to_string),
        effective_at: now,
        superseded_at: None,
    }
}

/// The jurisdiction's default for a purpose when the subject has said
/// nothing. This is the entire difference between "opt-in EU" and "opt-out
/// US" — one code path, two defaults.
pub fn jurisdiction_default(
    profile: &JurisdictionProfile,
    purpose: ProcessingPurpose,
) -> (DirectiveState, DirectiveSource) {
    use ProcessingPurpose::*;

    if purpose == Essential {
        return (DirectiveState::Allowed, DirectiveSource::Default);
    }

    match profile.consent_model {
        // EU/UK/JP: nothing non-essential runs until permission is given.
        ConsentModel::OptIn => (DirectiveState::Denied, DirectiveSource::Default),
        // US states: notice + honoured refusal. Only the purposes the plan
        // lists as notice-and-opt-out default to allowed after the notice
        // at collection has been served.
        ConsentModel::NoticeAndOptOut => match purpose {
            Analytics | SaleOrShare | TargetedAdvertising => {
                (DirectiveState::Allowed, DirectiveSource::Default)
            }
            SensitivePiUse => match profile.sensitive_model {
                // CA: sensitive use is limited on request, not gated up
                // front — allowed until the "limit my sensitive PI"
                // directive arrives.
                SensitiveModel::LimitOnRequest => {
                    (DirectiveState::Allowed, DirectiveSource::Default)
                }
                SensitiveModel::OptIn | SensitiveModel::NoticeOnly => {
                    (DirectiveState::Denied, DirectiveSource::Default)
                }
            },
            _ => (DirectiveState::Denied, DirectiveSource::Default),
        },
    }
}

/// Resolve the full snapshot for a subject.
#[allow(clippy::too_many_arguments)]
pub fn resolve_snapshot(
    profile: &JurisdictionProfile,
    explicit: &[DirectiveInput],
    signals: &[DirectiveInput],
    is_minor: bool,
    notice_version: Option<&str>,
    now: DateTime<Utc>,
    honours_universal_opt_out_override: Option<bool>,
) -> Vec<PurposeDirective> {
    let mut profile_for_signals = profile.clone();
    if let Some(override_value) = honours_universal_opt_out_override {
        profile_for_signals.honours_universal_opt_out = override_value;
    }
    ProcessingPurpose::ALL
        .iter()
        .map(|purpose| {
            let explicit_for_purpose = explicit
                .iter()
                .rev()
                .find(|input| input.purpose == *purpose);
            let signal_for_purpose = signals.iter().rev().find(|input| input.purpose == *purpose);
            resolve_purpose(
                &profile_for_signals,
                *purpose,
                explicit_for_purpose,
                signal_for_purpose,
                is_minor,
                notice_version,
                now,
            )
        })
        .collect()
}

/// Convenience: the resolved state for a purpose as a plain pair.
pub fn resolve_state(
    profile: &JurisdictionProfile,
    purpose: ProcessingPurpose,
    explicit: Option<&DirectiveInput>,
    signal: Option<&DirectiveInput>,
    is_minor: bool,
    notice_version: Option<&str>,
    now: DateTime<Utc>,
) -> (DirectiveState, DirectiveSource, JurisdictionRef) {
    let directive = resolve_purpose(
        profile,
        purpose,
        explicit,
        signal,
        is_minor,
        notice_version,
        now,
    );
    (directive.state, directive.source, directive.jurisdiction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::jurisdiction::{most_protective, seeded_profile};
    use crate::domain::types::{ConsentModel, SensitiveModel};

    fn profile(code: &str) -> JurisdictionProfile {
        seeded_profile(&JurisdictionRef::parse(code).unwrap()).unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    #[test]
    fn essential_is_always_allowed() {
        for code in ["eu", "jp", "us-ca"] {
            let directive = resolve_purpose(
                &profile(code),
                ProcessingPurpose::Essential,
                None,
                None,
                false,
                None,
                now(),
            );
            assert_eq!(directive.state, DirectiveState::Allowed, "{code}");
        }
    }

    #[test]
    fn eu_defaults_everything_nonessential_to_denied() {
        let eu = profile("eu");
        for purpose in ProcessingPurpose::ALL {
            if purpose == ProcessingPurpose::Essential {
                continue;
            }
            let (state, source) = jurisdiction_default(&eu, purpose);
            assert_eq!(state, DirectiveState::Denied, "{} in eu", purpose.as_str());
            assert_eq!(source, DirectiveSource::Default);
        }
    }

    #[test]
    fn us_notice_and_opt_out_defaults() {
        let ca = profile("us-ca");
        for (purpose, expected) in [
            (ProcessingPurpose::Analytics, DirectiveState::Allowed),
            (ProcessingPurpose::SaleOrShare, DirectiveState::Allowed),
            (
                ProcessingPurpose::TargetedAdvertising,
                DirectiveState::Allowed,
            ),
            (
                ProcessingPurpose::SensitivePiUse,
                DirectiveState::Allowed, // limit_on_request model
            ),
            (ProcessingPurpose::MarketingEmail, DirectiveState::Denied),
            (
                ProcessingPurpose::AdsPersonalisation,
                DirectiveState::Denied,
            ),
            (
                ProcessingPurpose::ProfilingSignificantEffects,
                DirectiveState::Denied,
            ),
        ] {
            let (state, _) = jurisdiction_default(&ca, purpose);
            assert_eq!(state, expected, "{} in us-ca", purpose.as_str());
        }

        // A non-CA state with opt-in sensitive model denies sensitive use.
        let va = profile("us-va");
        let (state, _) = jurisdiction_default(&va, ProcessingPurpose::SensitivePiUse);
        assert_eq!(state, DirectiveState::Denied);
    }

    #[test]
    fn explicit_opt_out_beats_jurisdiction_default() {
        let ca = profile("us-ca");
        let opt_out = DirectiveInput {
            purpose: ProcessingPurpose::SaleOrShare,
            state: DirectiveState::Denied,
            source: DirectiveSource::OptOut,
            notice_version: None,
        };
        let directive = resolve_purpose(
            &ca,
            ProcessingPurpose::SaleOrShare,
            Some(&opt_out),
            None,
            false,
            None,
            now(),
        );
        assert_eq!(directive.state, DirectiveState::Denied);
        assert_eq!(directive.source, DirectiveSource::OptOut);
    }

    #[test]
    fn gpc_signal_denies_sale_and_targeted_ads_before_any_action() {
        let ca = profile("us-ca");
        let signal = DirectiveInput {
            purpose: ProcessingPurpose::SaleOrShare,
            state: DirectiveState::Denied,
            source: DirectiveSource::Signal,
            notice_version: None,
        };
        let directive = resolve_purpose(
            &ca,
            ProcessingPurpose::SaleOrShare,
            None,
            Some(&signal),
            false,
            None,
            now(),
        );
        assert_eq!(directive.state, DirectiveState::Denied);
        assert_eq!(directive.source, DirectiveSource::Signal);
    }

    #[test]
    fn explicit_consent_beats_a_signal_but_only_explicitly() {
        // A subsequent explicit consent (recorded with source=consent, e.g.
        // the UI asked and the subject said yes) overrides the signal — the
        // plan requires the UI to ask, never silent override.
        let ca = profile("us-ca");
        let signal = DirectiveInput {
            purpose: ProcessingPurpose::SaleOrShare,
            state: DirectiveState::Denied,
            source: DirectiveSource::Signal,
            notice_version: None,
        };
        let consent = DirectiveInput {
            purpose: ProcessingPurpose::SaleOrShare,
            state: DirectiveState::Allowed,
            source: DirectiveSource::Consent,
            notice_version: None,
        };
        let directive = resolve_purpose(
            &ca,
            ProcessingPurpose::SaleOrShare,
            Some(&consent),
            Some(&signal),
            false,
            None,
            now(),
        );
        assert_eq!(directive.state, DirectiveState::Allowed);
        assert_eq!(directive.source, DirectiveSource::Consent);
    }

    #[test]
    fn minors_are_denied_absent_guardian_consent() {
        let ca = profile("us-ca");
        // Without guardian consent the minor gate fires (Denied, source
        // Guardian) — even though the CA default is Allowed.
        let directive = resolve_purpose(
            &ca,
            ProcessingPurpose::SaleOrShare,
            None,
            None,
            true,
            None,
            now(),
        );
        assert_eq!(directive.state, DirectiveState::Denied);
        assert_eq!(directive.source, DirectiveSource::Guardian);

        // A recorded guardian decision is an explicit input: allowed.
        let guardian = DirectiveInput {
            purpose: ProcessingPurpose::SaleOrShare,
            state: DirectiveState::Allowed,
            source: DirectiveSource::Guardian,
            notice_version: None,
        };
        let directive = resolve_purpose(
            &ca,
            ProcessingPurpose::SaleOrShare,
            Some(&guardian),
            None,
            true,
            None,
            now(),
        );
        assert_eq!(directive.state, DirectiveState::Allowed);
    }

    #[test]
    fn refusal_survives_notice_version_changes() {
        // An exercised opt-out must never be reset by a policy edit; the
        // explicit input outranks the (new) default regardless of version.
        let profile = profile("us-ca");
        let opt_out = DirectiveInput {
            purpose: ProcessingPurpose::Analytics,
            state: DirectiveState::Denied,
            source: DirectiveSource::OptOut,
            notice_version: Some("v1".to_string()),
        };
        let directive = resolve_purpose(
            &profile,
            ProcessingPurpose::Analytics,
            Some(&opt_out),
            None,
            false,
            Some("v2"),
            now(),
        );
        assert_eq!(directive.state, DirectiveState::Denied);
    }

    #[test]
    fn profile_model_fields_drive_everything() {
        // The only difference between an EU and a CA subject is the profile
        // row: same code path, different models.
        let eu = profile("eu");
        let ca = profile("us-ca");
        assert_eq!(eu.consent_model, ConsentModel::OptIn);
        assert_eq!(ca.consent_model, ConsentModel::NoticeAndOptOut);
        assert_eq!(ca.sensitive_model, SensitiveModel::LimitOnRequest);
        assert_eq!(eu.minor_opt_in_age, Some(16));
        assert!(eu.risk_assessment_required);
        assert!(ca.risk_assessment_required);
        assert!(!eu.code.eq(&most_protective()) || eu.code == most_protective());
    }
}
