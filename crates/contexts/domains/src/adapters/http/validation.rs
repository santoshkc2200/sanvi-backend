//! TASK-027 (FR-1127): field addressing for the domains context's
//! validation errors.
//!
//! [`validation_issue`] classifies every variant of [`DomainsError`]:
//! `Some((detail, issues))` for the variants caused by an invalid input
//! value — `detail` byte-identical to what the flat mapping emitted, so
//! addressing an error never rewrites what a client already renders — and
//! `None` for every other class (state conflicts, availability,
//! infrastructure). The match is exhaustive, so a new variant cannot join
//! the contract without deciding whether it names a field.
//! [`error_samples`] constructs one sample per variant for the contract
//! enumeration gate (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::{CustomDomainId, DomainOrderId, DomainsError};

/// The flat rejection text plus the fields it addresses, or `None` when
/// the variant is not an invalid-input rejection.
pub fn validation_issue(err: &DomainsError) -> Option<(String, Vec<FieldIssue>)> {
    // One field, one message: the message is the same text the flat
    // mapping put in `detail`.
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        // Every hostname-shaped rejection belongs to the `hostname` input,
        // wherever in the claim/search/order journey it was raised.
        DomainsError::InvalidHostname(detail) => addressed("hostname", detail),
        DomainsError::HostnameReserved(detail) => addressed("hostname", detail),
        DomainsError::HostnameOnPlatformDomain(detail) => addressed("hostname", detail),
        DomainsError::UnsupportedTld(detail) => addressed("hostname", detail),
        DomainsError::DomainUnavailable(detail) => addressed("hostname", detail),
        DomainsError::HostnameAlreadyClaimed(_)
        | DomainsError::HostnameAlreadyRegistered(_)
        | DomainsError::UnknownDomain(_)
        | DomainsError::UnknownOrder(_)
        | DomainsError::DomainNotOwned(_)
        | DomainsError::IllegalTransition { .. }
        | DomainsError::DomainNotLive(_)
        | DomainsError::PrimaryAlreadyExists
        | DomainsError::DomainRemoved
        | DomainsError::VerificationThrottled { .. }
        | DomainsError::VerificationUnavailable(_)
        | DomainsError::NotEntitled { .. }
        | DomainsError::Registrar(_)
        // State/policy refusals mapped to 400 rather than 409 by the
        // existing contract: no input field is invalid, so there is no
        // field to name.
        | DomainsError::OrderNotModifiable(_)
        | DomainsError::OrderNonRefundable
        | DomainsError::DnsProvider(_)
        | DomainsError::CertificateAuthority(_)
        | DomainsError::NoCertificate
        | DomainsError::Charge(_)
        | DomainsError::Refund(_)
        | DomainsError::TlsDenied(_)
        | DomainsError::TlsNotConfigured
        | DomainsError::Probe(_)
        | DomainsError::Storage(_)
        | DomainsError::Cache(_)
        | DomainsError::Outbox(_)
        | DomainsError::Notification(_)
        | DomainsError::IdGeneration(_) => None,
    }
}

/// One constructed sample per variant, labelled with the variant name. The
/// contract enumeration gate walks this and fails on a validation shape
/// whose issue list is empty.
pub fn error_samples() -> Vec<(&'static str, DomainsError)> {
    use chrono::TimeZone as _;
    let domain_id = CustomDomainId::try_from(910_401_001_i64).expect("fixed id");
    let order_id = DomainOrderId::try_from(910_401_002_i64).expect("fixed id");
    let next = chrono::Utc
        .timestamp_opt(1_800_000_000, 0)
        .single()
        .expect("fixed timestamp");
    vec![
        (
            "InvalidHostname",
            DomainsError::InvalidHostname("bad host".to_string()),
        ),
        (
            "HostnameReserved",
            DomainsError::HostnameReserved("sanvi.dev".to_string()),
        ),
        (
            "HostnameOnPlatformDomain",
            DomainsError::HostnameOnPlatformDomain("shop.sanvi.dev".to_string()),
        ),
        (
            "HostnameAlreadyClaimed",
            DomainsError::HostnameAlreadyClaimed("acme.com".to_string()),
        ),
        (
            "HostnameAlreadyRegistered",
            DomainsError::HostnameAlreadyRegistered("acme.com".to_string()),
        ),
        ("UnknownDomain", DomainsError::UnknownDomain(domain_id)),
        ("UnknownOrder", DomainsError::UnknownOrder(order_id)),
        ("DomainNotOwned", DomainsError::DomainNotOwned(domain_id)),
        (
            "IllegalTransition",
            DomainsError::IllegalTransition {
                from: "pending".to_string(),
                to: "live".to_string(),
            },
        ),
        (
            "DomainNotLive",
            DomainsError::DomainNotLive("acme.com".to_string()),
        ),
        ("PrimaryAlreadyExists", DomainsError::PrimaryAlreadyExists),
        ("DomainRemoved", DomainsError::DomainRemoved),
        (
            "VerificationThrottled",
            DomainsError::VerificationThrottled { next },
        ),
        (
            "VerificationUnavailable",
            DomainsError::VerificationUnavailable("dns provider down".to_string()),
        ),
        (
            "NotEntitled",
            DomainsError::NotEntitled {
                feature: "domains.custom".to_string(),
            },
        ),
        (
            "Registrar",
            DomainsError::Registrar("registry timeout".to_string()),
        ),
        (
            "UnsupportedTld",
            DomainsError::UnsupportedTld(".invalid".to_string()),
        ),
        (
            "DomainUnavailable",
            DomainsError::DomainUnavailable("acme.com".to_string()),
        ),
        (
            "DnsProvider",
            DomainsError::DnsProvider("api error".to_string()),
        ),
        (
            "CertificateAuthority",
            DomainsError::CertificateAuthority("acme error".to_string()),
        ),
        ("NoCertificate", DomainsError::NoCertificate),
        ("Charge", DomainsError::Charge("declined".to_string())),
        ("Refund", DomainsError::Refund("failed".to_string())),
        (
            "OrderNotModifiable",
            DomainsError::OrderNotModifiable("pending".to_string()),
        ),
        ("OrderNonRefundable", DomainsError::OrderNonRefundable),
        ("TlsDenied", DomainsError::TlsDenied("policy".to_string())),
        ("TlsNotConfigured", DomainsError::TlsNotConfigured),
        ("Probe", DomainsError::Probe("timeout".to_string())),
        ("Storage", DomainsError::Storage("db down".to_string())),
        ("Cache", DomainsError::Cache("miss".to_string())),
        ("Outbox", DomainsError::Outbox("full".to_string())),
        (
            "Notification",
            DomainsError::Notification("send failed".to_string()),
        ),
        (
            "IdGeneration",
            DomainsError::IdGeneration(sanvi_kernel::IdGenerationError::OutOfRange),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every validation shape names its field with the unchanged flat
    /// message, and every non-validation shape stays unclassified.
    #[test]
    fn validation_shapes_carry_fields_non_validation_shapes_do_not() {
        let samples = error_samples();
        assert!(
            samples.len() >= 33,
            "the domains error enum grew — add the new variant to error_samples \
             and classify it in validation_issue"
        );
        for (name, err) in &samples {
            if let Some((detail, issues)) = validation_issue(err) {
                assert!(
                    !issues.is_empty(),
                    "{name} classifies as validation but carries no field"
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

    #[test]
    fn hostname_rejections_name_the_hostname_field() {
        let err = DomainsError::InvalidHostname("-bad".to_string());
        let (detail, issues) = validation_issue(&err).expect("validation");
        assert_eq!(detail, "-bad");
        assert_eq!(issues[0].field, "hostname");
        assert_eq!(issues[0].message, "-bad");
    }

    #[test]
    fn state_and_infrastructure_shapes_stay_unclassified() {
        for (name, err) in error_samples() {
            if matches!(
                err,
                DomainsError::Storage(_)
                    | DomainsError::PrimaryAlreadyExists
                    | DomainsError::OrderNonRefundable
                    | DomainsError::Registrar(_)
            ) {
                assert!(
                    validation_issue(&err).is_none(),
                    "{name} is not an invalid-input rejection"
                );
            }
        }
    }
}
