//! `sanvi-theming` — the theming bounded context (phase 07).
//!
//! A theme is an installable, versioned artifact described by a validated
//! manifest — declarative only, no code. Tenants pin a version, override
//! semantic tokens, adjust unlocked layouts, upload brand assets and (with
//! an entitlement) store sanitised custom CSS. Resolution produces one
//! `ResolvedTheme` document (merged tokens + a precompiled CSS
//! custom-property block) that the frontend consumes for SSR, cached with
//! event-driven invalidation and a fail-closed digest pin.
//!
//! Layering: `domain` (pure), `application` (use cases), `adapters`
//! (sqlx/redis/actix/file storage), wired by the composition root in
//! `apps/api`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::ThemingConfig;
pub use domain::{
    ArtifactStore, BrandAsset, BrandAssetKind, BrandAssetStore, BrandAssets, Clock,
    ContrastViolation, EventPublisher, FontSpec, LayoutOverride, LayoutSpec, LocalizedName,
    Manifest, ResolvedLayout, ResolvedTenantRef, ResolvedTheme, SanitizedCss,
    TenantContextResolver, TenantTheme, TenantThemeRepository, TenantThemeSnapshot, Theme,
    ThemeFeatureGate, ThemeRepository, ThemeState, ThemeStatus, ThemeVersion, ThemeVisibility,
    ThemingError, ThemingEvent, ThemingEventName, TokenModes, TokenNode, TokenValue,
    apply_overrides, check_theme, contrast_ratio, emit_css_vars, flatten, merge_tokens,
    parse_hex_color, parse_token_document, resolution_etag, sanitize_css, sign_preview_token,
    validate_asset, verify_preview_token,
};

/// Wire theming routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::routes::{
        theming_platform_routes, theming_public_routes, theming_tenant_scope_routes,
    };
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::routes::PERMISSIONED_ROUTES;
}
