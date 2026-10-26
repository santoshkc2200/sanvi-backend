//! Conservative breach review tasks: encryption alone cannot close an obligation.

use sanvi_privacy::adapters::RuleBasedObligationCalculator;
use sanvi_privacy::domain::{
    AffectedSet, BreachIncident, BreachObligationCalculator, DataClass, JurisdictionRef,
    NotificationObligation, ObligationAudience, seeded_profile, seeded_profiles,
};

fn fixed_discovery() -> chrono::DateTime<chrono::Utc> {
    // A Wednesday: the tenant clock's business-day math has a weekend to
    // skip, so a naive +2 days would be visibly wrong here.
    chrono::DateTime::parse_from_rfc3339("2026-10-07T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

/// The fixture: 300 subjects across eu + three US states, two tenants,
/// encrypted at rest. 300 crosses Texas's 250 threshold and sits below
/// every 500 one.
fn fixture(encrypted_at_rest: bool, subjects: u64) -> BreachIncident {
    BreachIncident {
        id: sanvi_privacy::domain::BreachIncidentId::try_from(42_i64).unwrap(),
        discovered_at: fixed_discovery(),
        contained_at: None,
        affected: AffectedSet {
            subjects_by_jurisdiction: Default::default(),
            subjects,
            tenants: 2,
            jurisdictions: vec![
                JurisdictionRef::parse("eu").unwrap(),
                JurisdictionRef::parse("us-ca").unwrap(),
                JurisdictionRef::parse("us-co").unwrap(),
                JurisdictionRef::parse("us-tx").unwrap(),
            ],
        },
        data_classes: vec![DataClass::IdentityProfile],
        encrypted_at_rest,
        notes: Some("TASK-029 fixture".to_string()),
        created_by: None,
    }
}

fn profiles() -> Vec<sanvi_privacy::domain::JurisdictionProfile> {
    let codes = ["eu", "us-ca", "us-co", "us-tx"];
    codes
        .iter()
        .map(|c| seeded_profile(&JurisdictionRef::parse(c).unwrap()).expect("seeded profile"))
        .collect()
}

fn find<'a>(
    obligations: &'a [NotificationObligation],
    jurisdiction: &str,
    audience: ObligationAudience,
) -> Option<&'a NotificationObligation> {
    obligations
        .iter()
        .find(|o| o.jurisdiction.as_str() == jurisdiction && o.audience == audience)
}

#[test]
fn the_multi_state_fixture_produces_exactly_the_rule_set_obligations() {
    let calculator = RuleBasedObligationCalculator::new();
    let obligations = calculator.compute(&fixture(true, 300), &profiles(), fixed_discovery());

    assert_eq!(obligations.len(), 10, "obligations: {obligations:?}");

    // The EU authority deadline is discovery + 72 hours — from discovery,
    // never from when the assessment ran.
    let eu_authority = find(&obligations, "eu", ObligationAudience::Authority)
        .expect("EU joins the obligation set for one subject");
    assert_eq!(
        eu_authority.due_at,
        fixed_discovery() + chrono::Duration::hours(72),
        "GDPR clock is 72 hours from discovery"
    );
    assert!(!eu_authority.suppressed_by_encryption);
    assert_eq!(
        find(&obligations, "eu", ObligationAudience::Consumer)
            .unwrap()
            .due_at,
        fixed_discovery()
    );

    // Colorado: no safe harbour, so the 30-day consumer notice stands.
    let co_consumer =
        find(&obligations, "us-co", ObligationAudience::Consumer).expect("CO consumer notice");
    assert_eq!(
        co_consumer.due_at,
        fixed_discovery() + chrono::Duration::days(30),
        "CPA consumer clock is 30 days from discovery"
    );
    assert!(
        !co_consumer.suppressed_by_encryption,
        "encryption at rest is not evidence that exposed data or keys were safe"
    );

    // Qualitative consumer deadlines are immediate review tasks; thresholds
    // still gate authority reporting using local counts when available.
    assert!(find(&obligations, "us-ca", ObligationAudience::Authority).is_none());
    assert_eq!(
        find(&obligations, "us-tx", ObligationAudience::Authority)
            .unwrap()
            .due_at,
        fixed_discovery()
    );
    for code in ["us-ca", "us-tx"] {
        assert_eq!(
            find(&obligations, code, ObligationAudience::Consumer)
                .unwrap()
                .due_at,
            fixed_discovery()
        );
    }
    for code in ["eu", "us-ca", "us-co", "us-tx"] {
        let tenant = find(&obligations, code, ObligationAudience::Tenant).unwrap();
        assert_eq!(tenant.due_at, fixed_discovery());
        assert!(!tenant.suppressed_by_encryption);
    }
}

#[test]
fn unencrypted_the_eu_obligation_stands() {
    let calculator = RuleBasedObligationCalculator::new();
    let obligations = calculator.compute(&fixture(false, 300), &profiles(), fixed_discovery());
    let eu_authority = find(&obligations, "eu", ObligationAudience::Authority).unwrap();
    assert!(
        !eu_authority.suppressed_by_encryption,
        "without encryption the 72h obligation is live"
    );
}

#[test]
fn authority_thresholds_decide_real_deadlines() {
    let calculator = RuleBasedObligationCalculator::new();
    // Give California a deadline (the seed has none) so the threshold
    // branch has something to gate, keeping the seed's 500 threshold.
    let mut ca = seeded_profile(&JurisdictionRef::parse("us-ca").unwrap()).unwrap();
    ca.breach_rules.authority_due_days = Some(5);

    let below = fixture(true, 499);
    let mut ca_only = below.clone();
    ca_only.affected.jurisdictions = vec![JurisdictionRef::parse("us-ca").unwrap()];
    let obligations = calculator.compute(&ca_only, &[ca.clone()], fixed_discovery());
    assert!(
        find(&obligations, "us-ca", ObligationAudience::Authority).is_none(),
        "499 affected is below the 500 threshold: no authority obligation"
    );

    let at = fixture(true, 500);
    let mut at_only = at.clone();
    at_only.affected.jurisdictions = vec![JurisdictionRef::parse("us-ca").unwrap()];
    let obligations = calculator.compute(&at_only, &[ca], fixed_discovery());
    let authority = find(&obligations, "us-ca", ObligationAudience::Authority)
        .expect("500 meets the threshold");
    assert_eq!(
        authority.due_at,
        fixed_discovery() + chrono::Duration::days(5),
        "the mutated clock is honoured from discovery"
    );
}

#[test]
fn every_seeded_profile_is_drillable_by_the_fixture_shape() {
    // The fixture asserts four profiles; this guards the other five so a
    // new seeded profile with a rule set the fixture-shape cannot express
    // (e.g. an hour-level clock) is at least inventoried here.
    let calculator = RuleBasedObligationCalculator::new();
    let all = seeded_profiles();
    let incident = fixture(false, 1_000);
    let obligations = calculator.compute(&incident, &all, fixed_discovery());
    let profiled: usize = all
        .iter()
        .filter(|p| incident.affected.jurisdictions.contains(&p.code))
        .count();
    let tenant_rows = obligations
        .iter()
        .filter(|o| o.audience == ObligationAudience::Tenant)
        .count();
    assert_eq!(tenant_rows, profiled, "one tenant row per affected profile");
}

#[test]
fn local_counts_and_japan_staged_reports_are_preserved() {
    let calculator = RuleBasedObligationCalculator::new();
    let mut incident = fixture(true, 1_000);
    incident
        .affected
        .subjects_by_jurisdiction
        .insert("us-tx".to_string(), 10);
    let obligations = calculator.compute(&incident, &profiles(), fixed_discovery());
    assert!(find(&obligations, "us-tx", ObligationAudience::Authority).is_none());
    incident.affected.jurisdictions = vec![JurisdictionRef::parse("jp").unwrap()];
    let jp = seeded_profile(&incident.affected.jurisdictions[0]).unwrap();
    let obligations = calculator.compute(&incident, &[jp], fixed_discovery());
    assert_eq!(
        find(&obligations, "jp", ObligationAudience::Authority)
            .unwrap()
            .due_at,
        fixed_discovery()
    );
    assert_eq!(
        find(&obligations, "jp", ObligationAudience::AuthorityFinal)
            .unwrap()
            .due_at,
        fixed_discovery() + chrono::Duration::days(30)
    );
}
