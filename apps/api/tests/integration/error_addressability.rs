//! Error addressability enumeration (TASK-027, FR-1127).
//!
//! Every validation error the API can emit must carry the field path it
//! belongs to, so a client can build an error summary that links to the
//! offending input. This gate **enumerates** — each of the thirteen
//! HTTP-facing contexts contributes one constructed sample per error
//! variant plus its exhaustive field classifier, and the test fails on any
//! sample classified as validation whose field list is empty or unnamed.
//!
//! A classifier is exhaustive over its context's error enum (the compiler
//! forces a decision per variant), so a new validation shape cannot join a
//! context without being classified here. The planted-flat self-test at
//! the bottom proves this gate still fails on a flat validation error — a
//! gate that has never caught anything is a gate nobody has tested.
//!
//! The thirteen contexts are the ones mounting HTTP handlers (see
//! `crates/contexts/*/src/adapters/http/`); `system` and `notifications`
//! emit no validation errors of their own (probe fakes and an outbound
//! gateway respectively).

use sanvi_http::FieldIssue;

/// One row per context: its name and every error variant with the field
/// classification its HTTP mapping applies. `Some((detail, issues))` means
/// "this variant is an invalid-input rejection addressed by these fields".
type Inventory = Vec<(&'static str, Option<(String, Vec<FieldIssue>)>)>;

fn contexts() -> Vec<(&'static str, Inventory)> {
    let rows: Vec<(&'static str, Inventory)> = vec![
        (
            "access",
            sanvi_access::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_access::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "advertising",
            sanvi_advertising::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_advertising::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "audit",
            sanvi_audit::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_audit::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "billing",
            sanvi_billing::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_billing::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "domains",
            sanvi_domains::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_domains::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "identity",
            sanvi_identity::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_identity::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "localization",
            sanvi_localization::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_localization::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "media",
            sanvi_media::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_media::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "payments",
            sanvi_payments::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_payments::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "platform_admin",
            sanvi_platform_admin::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_platform_admin::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "privacy",
            sanvi_privacy::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_privacy::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "tenancy",
            sanvi_tenancy::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_tenancy::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
        (
            "theming",
            sanvi_theming::adapters::http::validation::error_samples()
                .into_iter()
                .map(|(name, err)| {
                    (
                        name,
                        sanvi_theming::adapters::http::validation::validation_issue(&err),
                    )
                })
                .collect(),
        ),
    ];
    rows
}

/// The gate predicate, shared with the self-test: every validation
/// classification must name at least one field with a non-empty path and a
/// non-empty message. Single-field rejections carry the unchanged flat
/// detail as the message (pinned per context); multi-field rejections
/// (the campaign violation tree) carry one message per field.
fn flat_failures(context: &str, inventory: &Inventory) -> Vec<String> {
    let mut failures = Vec::new();
    for (variant, classification) in inventory {
        match classification {
            Some((detail, issues)) if issues.is_empty() => {
                failures.push(format!(
                    "{context}::{variant} classifies as validation but addresses no field — \
                     name the offending input (FieldIssue::new(field, message))"
                ));
            }
            Some((detail, issues)) => {
                if detail.trim().is_empty() {
                    failures.push(format!("{context}::{variant} lost its flat detail message"));
                }
                for issue in issues {
                    if issue.field.trim().is_empty() {
                        failures.push(format!(
                            "{context}::{variant} addresses an empty field path"
                        ));
                    }
                    if issue.message.trim().is_empty() {
                        failures.push(format!(
                            "{context}::{variant} addresses a field with an empty message"
                        ));
                    }
                }
            }
            None => {}
        }
    }
    failures
}

#[test]
fn all_thirteen_http_contexts_are_enumerated() {
    let rows = contexts();
    let names: std::collections::BTreeSet<&str> = rows.iter().map(|(name, _)| *name).collect();
    let expected: std::collections::BTreeSet<&str> = [
        "access",
        "advertising",
        "audit",
        "billing",
        "domains",
        "identity",
        "localization",
        "media",
        "payments",
        "platform_admin",
        "privacy",
        "tenancy",
        "theming",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        names, expected,
        "the enumeration registry and the HTTP-facing context set disagree — \
         a context gained or lost its HTTP adapter without this gate noticing"
    );
    // Every context actually contributes samples: an empty inventory would
    // make the gate vacuously clean for that context.
    for (name, inventory) in &rows {
        assert!(
            !inventory.is_empty(),
            "{name} contributes no error samples — extend its validation module"
        );
    }
}

#[test]
fn every_validation_error_carries_its_field_path() {
    let mut failures = Vec::new();
    for (name, inventory) in contexts() {
        failures.extend(flat_failures(name, &inventory));
    }
    assert!(
        failures.is_empty(),
        "flat validation errors in the contract:\n  - {}",
        failures.join("\n  - ")
    );
}

/// The wizard check: a multi-step payload rejected at step three names the
/// step-three field, not the envelope. The advertising campaign tree is
/// the canonical shape (`ad_groups[i].ads[j].creative…`).
#[test]
fn wizard_payloads_name_the_step_three_field() {
    let inventory = contexts()
        .into_iter()
        .find(|(name, _)| *name == "advertising")
        .expect("advertising is registered")
        .1;
    let (_, classification) = inventory
        .iter()
        .find(|(variant, _)| *variant == "Validation")
        .expect("the advertising Validation variant is sampled");
    let (_, issues) = classification
        .as_ref()
        .expect("campaign validation is field-addressed");
    assert!(
        issues
            .iter()
            .any(|issue| issue.field.starts_with("ad_groups[")),
        "campaign violations must carry their ad_groups[i].ads[j] path: {issues:?}"
    );

    // The checkout wizard's flat messages name their input, not the body:
    // a bad amount addresses `amount_minor`, a bad currency `currency`.
    let payments = contexts()
        .into_iter()
        .find(|(name, _)| *name == "payments")
        .expect("payments is registered")
        .1;
    for (variant, classification) in &payments {
        if variant.starts_with("Invalid") {
            assert!(
                classification.is_some(),
                "{variant} is an input shape and must name its field"
            );
        }
    }
}

/// The self-test: the gate fails on a planted flat validation error. If
/// this ever passes, the enumeration above has been softened into
/// decoration.
#[test]
fn the_gate_fails_on_a_planted_flat_validation_error() {
    let planted: Inventory = vec![
        (
            "Addressed",
            Some((
                "detail".to_string(),
                vec![FieldIssue::new("hostname", "detail")],
            )),
        ),
        (
            "Planted flat",
            Some(("flat detail".to_string(), Vec::new())),
        ),
        (
            "Planted unnamed",
            Some(("detail".to_string(), vec![FieldIssue::new(" ", "detail")])),
        ),
        (
            "Planted mute",
            Some(("detail".to_string(), vec![FieldIssue::new("hostname", " ")])),
        ),
        ("NotValidation", None),
    ];
    let failures = flat_failures("self_test", &planted);
    assert_eq!(
        failures.len(),
        3,
        "exactly the planted flat ones fail: {failures:?}"
    );
    assert!(failures[0].contains("Planted flat"));
    assert!(failures[1].contains("Planted unnamed"));
    assert!(failures[2].contains("Planted mute"));
}
