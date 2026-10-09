//! TASK-027 (FR-1127): field addressing for the billing context's
//! validation errors. [`validation_issue`] classifies every variant of
//! [`BillingError`] — `Some((detail, issues))` for invalid-input shapes,
//! `None` otherwise, the match exhaustive. [`error_samples`] feeds the
//! contract enumeration gate (`apps/api/tests/…/error_addressability.rs`).
//!
//! The flat mapping emits the variant's `Display` as `detail`, so that is
//! what the field messages carry here too.

use sanvi_http::FieldIssue;

use crate::domain::BillingError;

pub fn validation_issue(err: &BillingError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed =
        |field: &str, err: &BillingError| Some(FieldIssue::addressing(field, &err.to_string()));
    match err {
        BillingError::InvalidPlanKey(_) => addressed("key", err),
        BillingError::InvalidPlanName(_) => addressed("name", err),
        BillingError::InvalidPlanTier(_) => addressed("tier", err),
        BillingError::InvalidPlanVisibility(_) => addressed("visibility", err),
        BillingError::InvalidPrice(detail) => price_issue(detail, err),
        BillingError::InvalidCurrency(_) => addressed("currency", err),
        BillingError::InvalidInterval(_) => addressed("interval", err),
        BillingError::InvalidEntitlement(inner) => entitlement_issue(inner, err),
        BillingError::InvalidWebhookPayload(_)
        | BillingError::PlanNotFound(_)
        | BillingError::PriceNotFound(_)
        | BillingError::PriceNotActive(_)
        | BillingError::PlanNotPublic(_)
        | BillingError::SubscriptionNotFound(_)
        | BillingError::SubscriptionExists
        | BillingError::SubscriptionNotActive
        | BillingError::PlanKeyTaken(_)
        | BillingError::InvalidWebhookSignature(_)
        | BillingError::UnknownWebhookEvent(_)
        | BillingError::WebhookProcessing(_)
        | BillingError::StripeUnauthorized
        | BillingError::StripeRateLimited
        | BillingError::StripeBadRequest { .. }
        | BillingError::StripeService { .. }
        | BillingError::StripeConnection { .. }
        | BillingError::TaxRegistrationMissing
        | BillingError::TenantNotFound(_)
        | BillingError::Conflict(_)
        | BillingError::Storage(_) => None,
    }
}

/// `InvalidPrice` serves several inputs; the message's leading phrase is
/// the key.
fn price_issue(detail: &str, err: &BillingError) -> Option<(String, Vec<FieldIssue>)> {
    let field = if detail.starts_with("unit_amount_minor ") {
        "unit_amount_minor"
    } else if detail.starts_with("stripe_price_id ") {
        "stripe_price_id"
    } else if detail.starts_with("stripe_product_id ") {
        "stripe_product_id"
    } else {
        return None;
    };
    Some(FieldIssue::addressing(field, &err.to_string()))
}

/// `InvalidEntitlement` carries the offending feature key (or a message
/// naming it); the grants map is keyed by feature.
fn entitlement_issue(inner: &str, err: &BillingError) -> Option<(String, Vec<FieldIssue>)> {
    let (field, message) = match inner.strip_prefix("negative limit for ") {
        Some(feature_key) => (format!("entitlements.{feature_key}.limit"), err.to_string()),
        None => (format!("entitlements.{inner}"), err.to_string()),
    };
    Some((message.clone(), vec![FieldIssue::new(field, message)]))
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, BillingError)> {
    vec![
        (
            "InvalidPlanKey",
            BillingError::InvalidPlanKey("Bad Key".to_string()),
        ),
        (
            "InvalidPlanName",
            BillingError::InvalidPlanName(String::new()),
        ),
        (
            "InvalidPlanTier",
            BillingError::InvalidPlanTier("platinum".to_string()),
        ),
        (
            "InvalidPlanVisibility",
            BillingError::InvalidPlanVisibility("secret".to_string()),
        ),
        (
            "InvalidPrice",
            BillingError::InvalidPrice("unit_amount_minor must be >= 0".to_string()),
        ),
        (
            "InvalidCurrency",
            BillingError::InvalidCurrency("euro".to_string()),
        ),
        (
            "InvalidInterval",
            BillingError::InvalidInterval("decade".to_string()),
        ),
        (
            "InvalidEntitlement",
            BillingError::InvalidEntitlement("advertising".to_string()),
        ),
        (
            "PlanNotFound",
            BillingError::PlanNotFound("acme".to_string()),
        ),
        (
            "PriceNotFound",
            BillingError::PriceNotFound("price_123".to_string()),
        ),
        (
            "PriceNotActive",
            BillingError::PriceNotActive("price_123".to_string()),
        ),
        (
            "PlanNotPublic",
            BillingError::PlanNotPublic("acme".to_string()),
        ),
        (
            "SubscriptionNotFound",
            BillingError::SubscriptionNotFound("910_601_009".to_string()),
        ),
        ("SubscriptionExists", BillingError::SubscriptionExists),
        ("SubscriptionNotActive", BillingError::SubscriptionNotActive),
        (
            "PlanKeyTaken",
            BillingError::PlanKeyTaken("acme".to_string()),
        ),
        (
            "InvalidWebhookPayload",
            BillingError::InvalidWebhookPayload("bad json".to_string()),
        ),
        (
            "InvalidWebhookSignature",
            BillingError::InvalidWebhookSignature("bad signature".to_string()),
        ),
        (
            "UnknownWebhookEvent",
            BillingError::UnknownWebhookEvent("e.t.c".to_string()),
        ),
        (
            "WebhookProcessing",
            BillingError::WebhookProcessing("handler blew up".to_string()),
        ),
        ("StripeUnauthorized", BillingError::StripeUnauthorized),
        ("StripeRateLimited", BillingError::StripeRateLimited),
        (
            "StripeBadRequest",
            BillingError::StripeBadRequest {
                detail: "no such customer".to_string(),
            },
        ),
        (
            "StripeService",
            BillingError::StripeService {
                detail: "500".to_string(),
            },
        ),
        (
            "StripeConnection",
            BillingError::StripeConnection {
                detail: "timeout".to_string(),
            },
        ),
        (
            "TaxRegistrationMissing",
            BillingError::TaxRegistrationMissing,
        ),
        (
            "TenantNotFound",
            BillingError::TenantNotFound("910_601_009".to_string()),
        ),
        (
            "Conflict",
            BillingError::Conflict("override race".to_string()),
        ),
        ("Storage", BillingError::Storage("db down".to_string())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_shapes_carry_fields_non_validation_shapes_do_not() {
        for (name, err) in error_samples() {
            match validation_issue(&err) {
                Some((detail, issues)) => {
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
                None => continue,
            }
        }
    }

    #[test]
    fn price_and_entitlement_messages_stay_pinned() {
        let price = BillingError::InvalidPrice("stripe_price_id must not be empty".to_string());
        let (_, issues) = validation_issue(&price).expect("price");
        assert_eq!(issues[0].field, "stripe_price_id");
        assert_eq!(
            issues[0].message,
            "invalid price: stripe_price_id must not be empty"
        );

        let grant = BillingError::InvalidEntitlement("negative limit for advertising".to_string());
        let (_, issues) = validation_issue(&grant).expect("entitlement");
        assert_eq!(issues[0].field, "entitlements.advertising.limit");

        let unknown = BillingError::InvalidPrice("something else".to_string());
        assert!(validation_issue(&unknown).is_none());
    }
}
