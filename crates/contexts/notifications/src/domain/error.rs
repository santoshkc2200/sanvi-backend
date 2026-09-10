use thiserror::Error;

/// Failures that can occur when communicating with the notification service.
#[derive(Debug, Clone, Error)]
pub enum NotificationError {
    #[error("{detail}")]
    Invalid { code: String, detail: String },
    #[error("{detail}")]
    Conflict { detail: String },
    #[error("notification-service unavailable: {detail}")]
    Unavailable { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_match_expectation() {
        let invalid = NotificationError::Invalid {
            code: "bad_request".to_string(),
            detail: "recipient is not a valid email".to_string(),
        };
        assert_eq!(invalid.to_string(), "recipient is not a valid email");

        let conflict = NotificationError::Conflict {
            detail: "idempotency key reused with different payload".to_string(),
        };
        assert_eq!(
            conflict.to_string(),
            "idempotency key reused with different payload"
        );

        let unavailable = NotificationError::Unavailable {
            detail: "connection refused".to_string(),
        };
        assert_eq!(
            unavailable.to_string(),
            "notification-service unavailable: connection refused"
        );
    }
}
