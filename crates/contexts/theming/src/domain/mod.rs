//! The theming context's domain: theme registry models, the DTCG token
//! engine, manifest validation, contrast/accessibility gates, the CSS
//! sanitizer, resolved-theme documents and the domain events.
//!
//! Layering contract: nothing under this module may reference sqlx, actix,
//! redis, reqwest or any other infrastructure crate.

pub mod assets;
pub mod contrast;
pub mod css;
pub mod error;
pub mod events;
pub mod manifest;
pub mod ports;
pub mod preview;
pub mod resolved;
pub mod sanitizer;
pub mod tenant_theme;
pub mod theme;
pub mod tokens;

pub use assets::{MAX_ASSET_BYTES, MAX_ASSET_DIMENSION, ValidatedAsset, validate_asset};
pub use contrast::{ContrastViolation, check_mode, check_theme, contrast_ratio};
pub use css::emit_css_vars;
pub use error::ThemingError;
pub use events::{ThemingEvent, ThemingEventName};
pub use manifest::{
    FontSpec, KNOWN_CAPABILITIES, LayoutSpec, LocalizedName, Manifest, ManifestAssets,
    THEME_API_VERSION, TokenRefs, validate_manifest,
};
pub use ports::{
    ArtifactStore, BrandAssetStore, Clock, EventPublisher, ResolvedTenantRef,
    TenantContextResolver, TenantThemeRepository, ThemeFeatureGate, ThemeRepository,
};
pub use preview::{PreviewTokenPayload, sign_preview_token, verify_preview_token};
pub use resolved::{
    BrandAssetUrls, CUSTOM_CSS_SCOPE_CLASS, ResolvedLayout, ResolvedTheme, ThemeAssetUrls,
    resolution_etag,
};
pub use sanitizer::{SanitizedCss, sanitize_css};
pub use tenant_theme::{
    BrandAsset, BrandAssetKind, BrandAssets, LayoutOverride, TenantTheme, TenantThemeSnapshot,
    ThemeState, validate_layout_overrides,
};
pub use theme::{DEFAULT_THEME_KEY, Theme, ThemeStatus, ThemeVersion, ThemeVisibility, TokenModes};
pub use tokens::{
    KNOWN_TOKEN_TYPES, TokenNode, TokenValue, apply_overrides, flatten, merge_tokens,
    parse_hex_color, parse_token_document,
};
