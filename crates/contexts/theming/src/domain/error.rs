//! The theming context's error vocabulary.

#[derive(Debug, thiserror::Error)]
pub enum ThemingError {
    #[error("unknown theme: {0}")]
    UnknownTheme(String),

    #[error("unknown theme version: {theme}@{version}")]
    UnknownThemeVersion { theme: String, version: String },

    #[error("theme version {theme}@{version} has been withdrawn")]
    ThemeVersionYanked { theme: String, version: String },

    #[error("theme version already exists: {theme}@{version}")]
    ThemeVersionExists { theme: String, version: String },

    #[error("theme is deprecated: {0}")]
    ThemeDeprecated(String),

    #[error("theme requires the {feature} entitlement")]
    ThemeNotEntitled { feature: String },

    #[error("theme api {found} is not compatible with the supported range {supported}")]
    ThemeApiMismatch { found: String, supported: String },

    #[error("invalid theme manifest: {0}")]
    InvalidManifest(String),

    #[error("invalid token document: {0}")]
    InvalidTokens(String),

    #[error("token override not allowed: {path}")]
    OverrideNotAllowed { path: String },

    #[error("token override type mismatch for {path}: expected {expected}, got {found}")]
    OverrideTypeMismatch {
        path: String,
        expected: String,
        found: String,
    },

    #[error("layout override not allowed: {0}")]
    LayoutOverrideNotAllowed(String),

    #[error("layout override requires the {0} entitlement")]
    LayoutOverrideNotEntitled(String),

    #[error("custom css requires the {0} entitlement")]
    CustomCssNotEntitled(String),

    #[error("custom css is invalid: {0}")]
    InvalidCustomCss(String),

    #[error("contrast check failed: {0}")]
    ContrastFailure(String),

    #[error("invalid theme state: {0}")]
    InvalidState(String),

    #[error("no previous revision to roll back to")]
    NothingToRollBack,

    #[error("invalid preview token")]
    InvalidPreviewToken,

    #[error("preview token expired")]
    PreviewTokenExpired,

    #[error("invalid brand asset: {0}")]
    InvalidBrandAsset(String),

    #[error("storage failure: {0}")]
    Storage(String),

    #[error("cache failure: {0}")]
    Cache(String),

    #[error("outbox failure: {0}")]
    Outbox(String),
}
