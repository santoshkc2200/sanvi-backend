use thiserror::Error;

/// Every failure this context's port can return. `code` carries
/// `hitox-media-service`'s own machine-readable error code through
/// unmodified — `course-media`'s `CourseApiError` already reads it, and
/// this context does not get to invent a competing vocabulary.
#[derive(Debug, Clone, Error)]
pub enum MediaError {
    #[error("{detail}")]
    BadRequest { code: String, detail: String },
    #[error("{detail}")]
    NotFound { code: String, detail: String },
    #[error("{detail}")]
    Conflict { code: String, detail: String },
    #[error("hitox-media-service unavailable: {detail}")]
    Unavailable { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_variant_keeps_its_own_message_in_display() {
        // BadRequest: detail only
        let err = MediaError::BadRequest {
            code: "invalid_input".to_string(),
            detail: "filename is required".to_string(),
        };
        assert_eq!(err.to_string(), "filename is required");

        // NotFound: detail only
        let err = MediaError::NotFound {
            code: "not_found".to_string(),
            detail: "asset abc123 does not exist".to_string(),
        };
        assert_eq!(err.to_string(), "asset abc123 does not exist");

        // Conflict: detail only
        let err = MediaError::Conflict {
            code: "already_exists".to_string(),
            detail: "asset with this id already exists".to_string(),
        };
        assert_eq!(err.to_string(), "asset with this id already exists");

        // Unavailable: includes the prefix
        let err = MediaError::Unavailable {
            detail: "service is temporarily offline".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "hitox-media-service unavailable: service is temporarily offline"
        );
    }
}
