//! The localization context's error vocabulary.

#[derive(Debug, thiserror::Error)]
pub enum LocalizationError {
    #[error("invalid locale tag: {0}")]
    InvalidLocale(String),

    #[error("unsupported locale: {0}")]
    UnsupportedLocale(String),

    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),

    #[error("unknown catalog domain: {0}")]
    UnknownDomain(String),

    #[error("unknown message key: {0}")]
    UnknownMessageKey(String),

    #[error("invalid override locale {locale} for key {key}: {detail}")]
    InvalidOverride {
        key: String,
        locale: String,
        detail: String,
    },

    #[error("invalid message pattern {key:?}: {detail}")]
    InvalidPattern { key: String, detail: String },

    #[error("unknown currency code: {0}")]
    UnknownCurrency(String),

    #[error("invalid address: {0}")]
    InvalidAddress(String),

    #[error("settings conflict: {0}")]
    SettingsConflict(String),

    #[error("storage failure: {0}")]
    Storage(String),

    #[error("outbox failure: {0}")]
    Outbox(String),
}
