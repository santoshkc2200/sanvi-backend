//! TASK-027 (FR-1127): field addressing for the audit context's validation
//! errors. [`validation_issue`] classifies every variant of [`AuditError`]
//! — `Some((detail, issues))` for invalid-input shapes, `None` otherwise,
//! the match exhaustive. [`error_samples`] feeds the contract enumeration
//! gate (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::AuditError;

pub fn validation_issue(err: &AuditError) -> Option<(String, Vec<FieldIssue>)> {
    let addressed = |field: &str, detail: &str| Some(FieldIssue::addressing(field, detail));
    match err {
        AuditError::InvalidActorType(_) => addressed("actor_type", err_text(err)),
        AuditError::InvalidAction(_) => addressed("action", err_text(err)),
        AuditError::InvalidCursor(_) => addressed("cursor", err_text(err)),
        AuditError::ChainMismatch { .. } | AuditError::Storage(_) => None,
    }
}

/// The flat detail the mapping emits today: the variant's inner string.
fn err_text(err: &AuditError) -> &str {
    match err {
        AuditError::InvalidActorType(detail)
        | AuditError::InvalidAction(detail)
        | AuditError::InvalidCursor(detail) => detail,
        _ => unreachable!("err_text is only called on classified variants"),
    }
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, AuditError)> {
    vec![
        (
            "InvalidActorType",
            AuditError::InvalidActorType("wizard".to_string()),
        ),
        (
            "InvalidAction",
            AuditError::InvalidAction("not an action".to_string()),
        ),
        (
            "InvalidCursor",
            AuditError::InvalidCursor("not-base64".to_string()),
        ),
        (
            "ChainMismatch",
            AuditError::ChainMismatch {
                entry_id: sanvi_kernel::SnowflakeId::try_from(910_701_001_i64).expect("fixed id"),
                position: 3,
                detail: "hash mismatch".to_string(),
            },
        ),
        ("Storage", AuditError::Storage("db down".to_string())),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_rejections_name_their_query_param() {
        let cases: &[(&str, AuditError, &str)] = &[
            (
                "InvalidActorType",
                AuditError::InvalidActorType("wizard".into()),
                "actor_type",
            ),
            (
                "InvalidAction",
                AuditError::InvalidAction(String::new()),
                "action",
            ),
            (
                "InvalidCursor",
                AuditError::InvalidCursor("zz".into()),
                "cursor",
            ),
        ];
        for (name, err, field) in cases {
            let (detail, issues) = validation_issue(err).expect(name);
            assert_eq!(issues[0].field, *field, "{name}");
            assert_eq!(issues[0].message, detail);
        }
        // Integrity and storage shapes carry no field.
        let storage = AuditError::Storage("db down".into());
        assert!(validation_issue(&storage).is_none());
    }

    #[test]
    fn every_variant_is_sampled() {
        assert_eq!(error_samples().len(), 5);
    }
}
