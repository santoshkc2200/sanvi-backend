//! The theme manifest (`theme.json`): a declarative, schema-checked
//! description of a theme artifact.
//!
//! The validator is fail-closed: unknown capabilities, foreign font sources,
//! path traversal in asset/token references and out-of-range `themeApi`
//! declarations all reject the artifact. A theme is declarative only — there
//! is deliberately no code slot in this schema.

use std::collections::BTreeMap;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::error::ThemingError;

/// The platform's implemented theme API version. A manifest declaring a
/// range that does not contain this version is rejected.
pub const THEME_API_VERSION: &str = "1.0.0";

/// Capabilities the platform knows how to honour. Anything else fails
/// closed: an unknown capability is a mistyped or future manifest, not
/// something to silently ignore.
pub const KNOWN_CAPABILITIES: &[&str] = &["dark-mode", "rtl-ready", "cjk-typography"];

/// Token-path roots the manifest may expose for tenant overrides.
pub const OVERRIDABLE_ROOTS: &[&str] =
    &["color", "font", "radius", "spacing", "shadow", "typography"];

const THEME_KEY_PATTERN: &str = "^[a-z][a-z0-9-]{1,63}$";
const LAYOUT_KEY_PATTERN: &str = "^[a-z][a-z0-9-]*(\\.[a-z0-9-]+)*$";

/// Localised display names in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LocalizedName {
    pub en: String,
    pub ja: String,
}

/// A font the theme uses. Self-hosted only — no remote `@import` is ever
/// possible through this schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FontSpec {
    pub family: String,
    pub subsets: Vec<String>,
    #[serde(rename = "source")]
    pub source: String,
}

/// A layout definition: named slots and whether tenants may restructure it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LayoutSpec {
    pub slots: Vec<String>,
    #[serde(default)]
    pub locked: bool,
}

/// Reference assets shipped with the theme (preview/screenshots).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ManifestAssets {
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub screenshots: Vec<String>,
}

/// The validated theme manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    #[serde(rename = "themeApi")]
    pub theme_api: String,
    pub name: LocalizedName,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub tokens: TokenRefs,
    #[serde(default)]
    pub overridable: Vec<String>,
    pub layouts: BTreeMap<String, LayoutSpec>,
    #[serde(default)]
    pub assets: ManifestAssets,
    #[serde(default)]
    pub fonts: Vec<FontSpec>,
}

/// Where the token documents live inside the artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TokenRefs {
    pub light: String,
    #[serde(default)]
    pub dark: Option<String>,
}

impl Manifest {
    /// Parse and validate a raw manifest document. Every violation is
    /// reported with the offending field.
    pub fn parse(value: &Value) -> Result<Self, ThemingError> {
        let manifest: Self = serde_json::from_value(value.clone())
            .map_err(|err| ThemingError::InvalidManifest(err.to_string()))?;

        if !is_match(THEME_KEY_PATTERN, &manifest.id) {
            return Err(ThemingError::InvalidManifest(format!(
                "id {id:?} must match {THEME_KEY_PATTERN}",
                id = manifest.id
            )));
        }
        Version::parse(&manifest.version).map_err(|err| {
            ThemingError::InvalidManifest(format!(
                "version {:?} is not semver: {err}",
                manifest.version
            ))
        })?;
        let declared = VersionReq::parse(&manifest.theme_api).map_err(|err| {
            ThemingError::InvalidManifest(format!(
                "themeApi {:?} is not a version range: {err}",
                manifest.theme_api
            ))
        })?;
        if !manifest.theme_api_compatible(&declared)? {
            return Err(ThemingError::ThemeApiMismatch {
                found: manifest.theme_api.clone(),
                supported: THEME_API_VERSION.to_string(),
            });
        }
        if manifest.name.en.trim().is_empty() || manifest.name.ja.trim().is_empty() {
            return Err(ThemingError::InvalidManifest(
                "name.en and name.ja must be non-empty".to_string(),
            ));
        }
        for capability in &manifest.capabilities {
            if !KNOWN_CAPABILITIES.contains(&capability.as_str()) {
                return Err(ThemingError::InvalidManifest(format!(
                    "unknown capability {capability:?} (known: {KNOWN_CAPABILITIES:?})"
                )));
            }
        }
        validate_relative_path(&manifest.tokens.light, "tokens.light")?;
        if let Some(dark) = &manifest.tokens.dark {
            validate_relative_path(dark, "tokens.dark")?;
        }
        for pattern in &manifest.overridable {
            validate_overridable(pattern)?;
        }
        for (layout, spec) in &manifest.layouts {
            if !is_match(LAYOUT_KEY_PATTERN, layout) {
                return Err(ThemingError::InvalidManifest(format!(
                    "layout key {layout:?} must match {LAYOUT_KEY_PATTERN}"
                )));
            }
            if spec.slots.is_empty() {
                return Err(ThemingError::InvalidManifest(format!(
                    "layout {layout} declares no slots"
                )));
            }
        }
        for font in &manifest.fonts {
            if font.family.trim().is_empty() {
                return Err(ThemingError::InvalidManifest(
                    "font family must be non-empty".to_string(),
                ));
            }
            if font.source != "self-hosted" {
                return Err(ThemingError::InvalidManifest(format!(
                    "font {} must be self-hosted (got source {:?})",
                    font.family, font.source
                )));
            }
        }
        for (field, value) in [("assets.preview", &manifest.assets.preview)] {
            if let Some(path) = value {
                validate_relative_path(path, field)?;
            }
        }
        for path in &manifest.assets.screenshots {
            validate_relative_path(path, "assets.screenshots")?;
        }

        Ok(manifest)
    }

    /// A declared `themeApi` range is compatible when it admits the version
    /// the platform implements. (A future platform bump keeps admitting
    /// themes that declared ranges covering it.)
    fn theme_api_compatible(&self, declared: &VersionReq) -> Result<bool, ThemingError> {
        let platform = Version::parse(THEME_API_VERSION).map_err(|err| {
            ThemingError::InvalidManifest(format!("platform theme api is broken: {err}"))
        })?;
        Ok(declared.matches(&platform))
    }

    /// Whether the manifest ships a dark token set.
    pub fn has_dark_mode(&self) -> bool {
        self.capabilities.iter().any(|cap| cap == "dark-mode") && self.tokens.dark.is_some()
    }

    /// Whether a token path may be overridden by the tenant.
    pub fn allows_override(&self, path: &str) -> bool {
        self.overridable
            .iter()
            .any(|pattern| pattern_matches(pattern, path))
    }

    /// Whether a layout may be restructured by the tenant (locked layouts
    /// never can).
    pub fn layout_unlocked(&self, layout: &str) -> bool {
        self.layouts.get(layout).is_some_and(|spec| !spec.locked)
    }
}

fn is_match(pattern: &str, input: &str) -> bool {
    // Regex-free on purpose: these patterns are anchors around tiny
    // character classes; keeping the dependency surface small matters more
    // than the generality a regex engine would buy here.
    match pattern {
        THEME_KEY_PATTERN => valid_theme_key(input),
        LAYOUT_KEY_PATTERN => valid_layout_key(input),
        _ => false,
    }
}

fn valid_theme_key(input: &str) -> bool {
    let mut chars = input.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let rest = chars.collect::<String>();
    rest.len() <= 63
        && rest
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_layout_key(input: &str) -> bool {
    if input.is_empty() || input.contains("..") {
        return false;
    }
    let mut segments = input.split('.');
    match segments.next() {
        Some(first) => {
            if first.is_empty() || !first.starts_with(|c: char| c.is_ascii_lowercase()) {
                return false;
            }
        }
        None => return false,
    }
    input
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
}

fn valid_overridable(pattern: &str) -> bool {
    // `group.path` or `group.path.*`: lowercase segments, `[a-z0-9-]` inside
    // segments, wildcard only as a trailing `.*`.
    let (head, wildcard) = match pattern.split_once(".*") {
        Some((head, "")) => (head, true),
        Some(_) => return false,
        None => (pattern, false),
    };
    let mut segments = head.split('.');
    let mut count = 0;
    match segments.next() {
        Some(root) => {
            if root.is_empty() || !root.chars().all(|c| c.is_ascii_lowercase()) {
                return false;
            }
            count += 1;
        }
        None => return false,
    }
    for segment in segments {
        count += 1;
        if segment.is_empty()
            || !segment.starts_with(|c: char| c.is_ascii_lowercase())
            || !segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return false;
        }
    }
    // A wildcard needs a group to apply to (`radius.*`); an exact path
    // needs at least one segment below the root (`spacing.scale`).
    count >= 2 || (wildcard && count == 1)
}

/// Pattern shapes: `color.brand.*` (group wildcard), `radius.md` (exact),
/// `color.background.primary` (deep exact).
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let Some((head, wildcard)) = pattern.split_once(".*") else {
        return pattern == path;
    };
    if !wildcard.is_empty() {
        return false;
    }
    path.starts_with(head)
        && path
            .strip_prefix(head)
            .is_some_and(|rest| rest.starts_with('.') && rest.len() > 1)
}

fn validate_overridable(pattern: &str) -> Result<(), ThemingError> {
    if !valid_overridable(pattern) {
        return Err(ThemingError::InvalidManifest(format!(
            "overridable entry {pattern:?} must be a dot path like `color.brand.*`"
        )));
    }
    let root = pattern.split('.').next().unwrap_or_default();
    if !OVERRIDABLE_ROOTS.contains(&root) {
        return Err(ThemingError::InvalidManifest(format!(
            "overridable entry {pattern:?} is not a semantic token group (allowed roots: {OVERRIDABLE_ROOTS:?})"
        )));
    }
    Ok(())
}

fn validate_relative_path(path: &str, field: &str) -> Result<(), ThemingError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains("..")
        || path
            .split(['/', '\\'])
            .any(|seg| seg.is_empty() || seg == ".")
    {
        return Err(ThemingError::InvalidManifest(format!(
            "{field} must be a clean relative path inside the artifact (got {path:?})"
        )));
    }
    Ok(())
}

/// Validate a manifest and return a typed error-free view of its
/// compatibility range — used by the publish pipeline before insertion.
pub fn validate_manifest(value: &Value) -> Result<Manifest, ThemingError> {
    Manifest::parse(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_json() -> Value {
        serde_json::json!({
            "id": "aurora",
            "version": "1.4.0",
            "themeApi": "^1.0.0",
            "name": { "en": "Aurora", "ja": "オーロラ" },
            "capabilities": ["dark-mode", "rtl-ready", "cjk-typography"],
            "tokens": { "light": "tokens/light.json", "dark": "tokens/dark.json" },
            "overridable": ["color.brand.*", "color.background.*", "font.family.*", "radius.*", "spacing.scale"],
            "layouts": {
                "storefront.home": { "slots": ["hero", "featured", "testimonials", "cta"] },
                "storefront.product": { "slots": ["gallery", "details", "related"] },
                "storefront.checkout": { "slots": ["summary", "payment"], "locked": true }
            },
            "assets": { "preview": "preview.png", "screenshots": ["shots/1.png"] },
            "fonts": [{ "family": "Noto Sans JP", "subsets": ["latin", "japanese"], "source": "self-hosted" }]
        })
    }

    #[test]
    fn parses_a_valid_manifest() {
        let manifest = Manifest::parse(&manifest_json()).unwrap();
        assert_eq!(manifest.id, "aurora");
        assert!(manifest.has_dark_mode());
        assert!(manifest.allows_override("color.brand.primary"));
        assert!(manifest.allows_override("font.family.base"));
        assert!(manifest.allows_override("spacing.scale"));
        assert!(!manifest.allows_override("color.focus.ring"));
        assert!(!manifest.allows_override("color.brand"));
        assert!(manifest.layout_unlocked("storefront.home"));
        assert!(!manifest.layout_unlocked("storefront.checkout"));
        assert!(!manifest.layout_unlocked("storefront.unknown"));
    }

    #[test]
    fn rejects_unknown_capabilities() {
        let mut value = manifest_json();
        value["capabilities"] = serde_json::json!(["dark-mode", "custom-js"]);
        let err = Manifest::parse(&value).unwrap_err();
        assert!(matches!(err, ThemingError::InvalidManifest(_)));
    }

    #[test]
    fn rejects_out_of_range_theme_api() {
        let mut value = manifest_json();
        value["themeApi"] = serde_json::json!("^2.0.0");
        let err = Manifest::parse(&value).unwrap_err();
        assert!(matches!(err, ThemingError::ThemeApiMismatch { .. }));

        let mut value = manifest_json();
        value["themeApi"] = serde_json::json!(">=1.1.0");
        assert!(matches!(
            Manifest::parse(&value).unwrap_err(),
            ThemingError::ThemeApiMismatch { .. }
        ));

        value["themeApi"] = serde_json::json!(">=1.0.0, <2.0.0");
        assert!(Manifest::parse(&value).is_ok());
    }

    #[test]
    fn rejects_traversal_and_remote_fonts() {
        let mut value = manifest_json();
        value["tokens"]["light"] = serde_json::json!("../../etc/passwd");
        assert!(matches!(
            Manifest::parse(&value).unwrap_err(),
            ThemingError::InvalidManifest(_)
        ));

        let mut value = manifest_json();
        value["fonts"][0]["source"] = serde_json::json!("google-fonts");
        assert!(matches!(
            Manifest::parse(&value).unwrap_err(),
            ThemingError::InvalidManifest(_)
        ));
    }

    #[test]
    fn rejects_non_semantic_override_roots() {
        let mut value = manifest_json();
        value["overridable"] = serde_json::json!(["color.brand.*", "border.*"]);
        assert!(matches!(
            Manifest::parse(&value).unwrap_err(),
            ThemingError::InvalidManifest(_)
        ));
    }
}
