//! TASK-027 (FR-1127): field addressing for the media context's validation
//! errors. `MediaError::BadRequest { code, detail }` is the one
//! input-shaped variant — the message's leading phrase keys the field.
//! [`validation_issue`] is exhaustive over [`MediaError`];
//! [`error_samples`] feeds the contract enumeration gate
//! (`apps/api/tests/…/error_addressability.rs`).

use sanvi_http::FieldIssue;

use crate::domain::MediaError;

pub fn validation_issue(err: &MediaError) -> Option<(String, Vec<FieldIssue>)> {
    let MediaError::BadRequest { detail, .. } = err else {
        return None;
    };
    let field = if detail.starts_with("kind ") {
        "kind"
    } else if detail.starts_with("filename") {
        "filename"
    } else if detail.starts_with("unsupported image format") {
        "content_type"
    } else if detail.starts_with("upload payload too large") {
        "size_bytes"
    } else {
        // Upstream (hitox) problem details flow through here too; an
        // unknown message stays flat rather than guessing a field.
        return None;
    };
    Some(FieldIssue::addressing(field, detail))
}

/// One constructed sample per variant for the contract enumeration gate.
pub fn error_samples() -> Vec<(&'static str, MediaError)> {
    vec![
        (
            "BadRequest",
            MediaError::BadRequest {
                code: "invalid_input".to_string(),
                detail: "kind must be one of image, video, audio".to_string(),
            },
        ),
        (
            "NotFound",
            MediaError::NotFound {
                code: "not_found".to_string(),
                detail: "asset abc123 does not exist".to_string(),
            },
        ),
        (
            "Conflict",
            MediaError::Conflict {
                code: "already_exists".to_string(),
                detail: "asset with this id already exists".to_string(),
            },
        ),
        (
            "Unavailable",
            MediaError::Unavailable {
                detail: "service is temporarily offline".to_string(),
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_input_rejections_name_their_field() {
        let cases: &[(&str, &str)] = &[
            ("kind must be one of image, video, audio", "kind"),
            ("filename is required", "filename"),
            ("unsupported image format", "content_type"),
            ("upload payload too large", "size_bytes"),
        ];
        for (message, field) in cases {
            let err = MediaError::BadRequest {
                code: "invalid_input".to_string(),
                detail: (*message).to_string(),
            };
            let (_, issues) = validation_issue(&err).unwrap_or_else(|| panic!("{message}"));
            assert_eq!(issues[0].field, *field, "{message}");
            assert_eq!(issues[0].message, *message);
        }
        // An upstream message with no known shape stays flat.
        let unknown = MediaError::BadRequest {
            code: "upstream".to_string(),
            detail: "something unexpected".to_string(),
        };
        assert!(validation_issue(&unknown).is_none());
    }

    #[test]
    fn non_bad_request_classes_stay_unclassified() {
        for (name, err) in error_samples() {
            if name != "BadRequest" {
                assert!(
                    validation_issue(&err).is_none(),
                    "{name} is not input validation"
                );
            }
        }
    }
}
