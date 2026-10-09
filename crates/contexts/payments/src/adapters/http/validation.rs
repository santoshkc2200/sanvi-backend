//! TASK-027 (FR-1127): field addressing for the payments context's
//! validation errors.
//!
//! [`validation_issue`] classifies every variant of [`PaymentsError`]:
//! `Some((detail, issues))` for the variants caused by an invalid input
//! value — `detail` byte-identical to what the flat mapping emitted — and
//! `None` for every other class (state conflicts, provider availability,
//! infrastructure). The match is exhaustive, so a new variant cannot join
//! the contract without deciding whether it names a field.
//! [`error_samples`] constructs one sample per variant for the contract
//! enumeration gate (`apps/api/tests/…/error_addressability.rs`).
//!
//! Two variants serve several inputs each (`InvalidMoney`,
//! `InvalidConnection`), so the field is keyed on the message's stable
//! leading phrase; [`tests::message_prefixes_stay_pinned`] fails when a
//! domain-side wording change moves a message off its key.

use sanvi_http::FieldIssue;

use crate::domain::error::PaymentsError;

/// The flat rejection text plus the fields it addresses, or `None` when
/// the variant is not an invalid-input rejection.
pub fn validation_issue(err: &PaymentsError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        PaymentsError::InvalidMoney(detail) => money_issue(detail),
        PaymentsError::InvalidConnection(detail) => connection_issue(detail),
        PaymentsError::InvalidCountry(detail) => addressed("country", detail),
        PaymentsError::CountryUnsupported(country) => addressed(
            "country",
            &format!("country {country} is not supported by this provider"),
        ),
        PaymentsError::CurrencyUnsupported { currency, detail } => addressed(
            "currency",
            &format!("currency {currency} is not supported by this connection: {detail}"),
        ),
        // The request is well-formed; the *amount* is just more than the
        // payment still allows. The client fixes it by lowering
        // `amount_minor`, so that is the field this names.
        PaymentsError::RefundExceedsRemaining {
            requested_minor,
            requested_currency,
            remaining_minor,
            remaining_currency,
        } => addressed(
            "amount_minor",
            &format!(
                "refund of {requested_minor} {requested_currency} exceeds remaining refundable {remaining_minor} {remaining_currency}"
            ),
        ),
        PaymentsError::ProviderUnavailable(_)
        | PaymentsError::ConnectionNotActive(_)
        | PaymentsError::CapabilityMissing { .. }
        | PaymentsError::IdempotencyConflict(_)
        | PaymentsError::NotEntitled(_)
        | PaymentsError::WebhookRejected(_)
        | PaymentsError::ProviderAuth
        | PaymentsError::ProviderRateLimited(_)
        | PaymentsError::ConnectionExists(_)
        | PaymentsError::DisconnectBlocked { .. }
        | PaymentsError::DomainNotAllowed(_)
        | PaymentsError::RateLimited(_)
        | PaymentsError::TaxPreflightFailed { .. }
        | PaymentsError::Storage(_)
        | PaymentsError::IdGeneration(_) => None,
    }
}

/// `InvalidMoney` serves several inputs; the message's leading phrase is
/// the key. Arithmetic misuse (`cannot add`, `portion`) is internal
/// flow — never a request field — and stays unclassified.
fn money_issue(detail: &str) -> Option<(String, Vec<FieldIssue>)> {
    let field = if detail.starts_with("currency ") {
        "currency"
    } else if detail.starts_with("amount ")
        || detail.starts_with("payment amount ")
        || detail.starts_with("checkout amount ")
        || detail.starts_with("refund amount ")
    {
        "amount_minor"
    } else {
        return None;
    };
    Some((
        detail.to_string(),
        vec![FieldIssue::new(field, detail.to_string())],
    ))
}

/// `InvalidConnection` serves several inputs; the message's leading phrase
/// is the key. State refusals (`is ... and cannot be`) and not-found
/// shapes (remapped to 404 by the handler) stay unclassified.
fn connection_issue(detail: &str) -> Option<(String, Vec<FieldIssue>)> {
    let field = if detail.starts_with("unknown payment status ") {
        "status"
    } else if detail.starts_with("unknown payout status ") {
        "payout_status"
    } else if detail.starts_with("merchant order reference ") {
        "reference"
    } else if detail.starts_with("invalid cursor") {
        "cursor"
    } else if detail.starts_with("refund reason ") {
        "reason"
    } else if detail.starts_with("provider ") || detail.starts_with("unknown provider ") {
        "provider"
    } else if detail.starts_with("locale ") {
        "locale"
    } else {
        return None;
    };
    Some((
        detail.to_string(),
        vec![FieldIssue::new(field, detail.to_string())],
    ))
}

/// One constructed sample per variant, labelled with the variant name. The
/// contract enumeration gate walks this and fails on a validation shape
/// whose issue list is empty.
pub fn error_samples() -> Vec<(&'static str, PaymentsError)> {
    vec![
        (
            "ProviderUnavailable",
            PaymentsError::ProviderUnavailable("stripe down".to_string()),
        ),
        (
            "ConnectionNotActive",
            PaymentsError::ConnectionNotActive("restricted".to_string()),
        ),
        (
            "CapabilityMissing",
            PaymentsError::CapabilityMissing {
                capability: "card_payments".to_string(),
                detail: "pending".to_string(),
            },
        ),
        (
            "CurrencyUnsupported",
            PaymentsError::CurrencyUnsupported {
                currency: "JPY".to_string(),
                detail: "not enabled".to_string(),
            },
        ),
        (
            "IdempotencyConflict",
            PaymentsError::IdempotencyConflict("key reuse".to_string()),
        ),
        (
            "InvalidMoney",
            PaymentsError::InvalidMoney("checkout amount must be positive, got 0 JPY".to_string()),
        ),
        (
            "InvalidConnection",
            PaymentsError::InvalidConnection("invalid cursor".to_string()),
        ),
        (
            "NotEntitled",
            PaymentsError::NotEntitled("payments.stripe_connect".to_string()),
        ),
        (
            "WebhookRejected",
            PaymentsError::WebhookRejected("bad signature".to_string()),
        ),
        ("ProviderAuth", PaymentsError::ProviderAuth),
        (
            "ProviderRateLimited",
            PaymentsError::ProviderRateLimited("retries exhausted".to_string()),
        ),
        (
            "InvalidCountry",
            PaymentsError::InvalidCountry("X1".to_string()),
        ),
        (
            "CountryUnsupported",
            PaymentsError::CountryUnsupported("XX".to_string()),
        ),
        (
            "ConnectionExists",
            PaymentsError::ConnectionExists("active".to_string()),
        ),
        (
            "DisconnectBlocked",
            PaymentsError::DisconnectBlocked {
                blockers: vec![crate::domain::DisconnectBlocker::OpenDisputes],
            },
        ),
        (
            "DomainNotAllowed",
            PaymentsError::DomainNotAllowed("evil.test".to_string()),
        ),
        (
            "RateLimited",
            PaymentsError::RateLimited("tenant bucket".to_string()),
        ),
        (
            "RefundExceedsRemaining",
            PaymentsError::RefundExceedsRemaining {
                requested_minor: 3000,
                requested_currency: "JPY".to_string(),
                remaining_minor: 2000,
                remaining_currency: "JPY".to_string(),
            },
        ),
        (
            "TaxPreflightFailed",
            PaymentsError::TaxPreflightFailed {
                reason: "tax_status_not_active".to_string(),
            },
        ),
        ("Storage", PaymentsError::Storage("db down".to_string())),
        (
            "IdGeneration",
            PaymentsError::IdGeneration("generator lost".to_string()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::money::Currency;

    /// Every validation shape names a field with the unchanged flat
    /// message.
    #[test]
    fn validation_shapes_carry_fields() {
        for (name, err) in error_samples() {
            if let Some((detail, issues)) = validation_issue(&err) {
                assert!(
                    !issues.is_empty(),
                    "{name} classifies as validation but has no field"
                );
                for issue in issues {
                    assert!(
                        !issue.field.is_empty(),
                        "{name} carries an empty field path"
                    );
                    assert_eq!(
                        issue.message, detail,
                        "{name} must not rewrite the flat message when addressing it"
                    );
                }
            }
        }
    }

    /// The field is keyed on the message's leading phrase; a domain-side
    /// wording change must move the key with it, so every key is pinned to
    /// the exact message the domain emits today.
    #[test]
    fn message_prefixes_stay_pinned() {
        // Money shapes.
        let currency_bad = Currency::parse("euro").unwrap_err();
        assert_eq!(field_of(&currency_bad).as_deref(), Some("currency"));
        let negative = crate::domain::Money::new(-1, "JPY").unwrap_err();
        assert_eq!(field_of(&negative).as_deref(), Some("amount_minor"));
        let zero_checkout = crate::domain::checkout::Checkout::new(
            crate::domain::CheckoutId::try_from(910_502_001_i64).expect("fixed id"),
            sanvi_kernel::TenantId::try_from(910_502_002_i64).expect("fixed id"),
            crate::domain::ConnectionId::try_from(910_502_003_i64).expect("fixed id"),
            crate::domain::MerchantOrderRef::parse("ord-1").expect("fixed ref"),
            crate::domain::Money::new(0, "JPY").expect("zero is constructible"),
            chrono::Utc::now(),
        )
        .unwrap_err();
        assert_eq!(field_of(&zero_checkout).as_deref(), Some("amount_minor"));
        // Arithmetic misuse is internal flow, never a request field.
        let cross = crate::domain::Money::new(100, "JPY")
            .expect("fixed money")
            .checked_add(&crate::domain::Money::new(100, "USD").expect("fixed money"))
            .unwrap_err();
        assert_eq!(field_of(&cross), None);

        // Connection shapes.
        let reference_empty = crate::domain::MerchantOrderRef::parse(" ").unwrap_err();
        assert_eq!(field_of(&reference_empty).as_deref(), Some("reference"));
        let status_unknown =
            PaymentsError::InvalidConnection("unknown payment status \"paided\"".to_string());
        assert_eq!(field_of(&status_unknown).as_deref(), Some("status"));
        let payout_unknown =
            PaymentsError::InvalidConnection("unknown payout status \"late\"".to_string());
        assert_eq!(field_of(&payout_unknown).as_deref(), Some("payout_status"));
        let cursor = PaymentsError::InvalidConnection("invalid cursor".to_string());
        assert_eq!(field_of(&cursor).as_deref(), Some("cursor"));
        // A state refusal carries no field.
        let state = PaymentsError::InvalidConnection(
            "payment 1 is succeeded and cannot be refunded".to_string(),
        );
        assert_eq!(field_of(&state), None);
    }

    fn field_of(err: &PaymentsError) -> Option<String> {
        validation_issue(err)
            .and_then(|(_, issues)| issues.into_iter().next().map(|issue| issue.field))
    }
}
