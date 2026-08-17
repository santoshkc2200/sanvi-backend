//! The token engine: DTCG token documents, deep merge and the tenant
//! override allow-list.
//!
//! Tokens arrive as nested DTCG groups (`{"color": {"brand": {"primary":
//! {"$value": "#2563eb", "$type": "color"}}}}`), are validated against a
//! closed set of token types and value shapes, then flattened to dotted
//! paths for override checking and CSS custom-property emission. Tenants may
//! only override *semantic* tokens the manifest's `overridable` list
//! exposes, and an override's `$type` must match the token it replaces.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::domain::error::ThemingError;
use crate::domain::manifest::Manifest;

/// Token types the platform understands. Anything else fails closed.
pub const KNOWN_TOKEN_TYPES: &[&str] = &[
    "color",
    "dimension",
    "fontFamily",
    "fontWeight",
    "number",
    "duration",
    "cubicBezier",
    "typography",
];

/// Units a dimension/duration token may use (no exotic or user-defined
/// units: they are a foot-gun for layout stability).
pub const KNOWN_UNITS: &[&str] = &[
    "px", "rem", "em", "pt", "%", "vh", "vw", "vmin", "vmax", "ch", "ms", "s",
];

/// Maximum nesting depth of a token document (guards absurd inputs and
/// keeps merge recursion bounded).
pub const MAX_DEPTH: usize = 10;

/// A leaf token (`$value` + optional `$type`/`$description`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenValue {
    #[serde(rename = "$value")]
    pub value: Value,
    #[serde(rename = "$type", default)]
    pub r#type: Option<String>,
    #[serde(rename = "$description", default)]
    pub description: Option<String>,
}

/// A token document node: a named group or a leaf token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum TokenNode {
    Group(BTreeMap<String, TokenNode>),
    Token(TokenValue),
}

impl TokenNode {
    pub fn is_token(&self) -> bool {
        matches!(self, Self::Token(_))
    }
}

/// Parse and validate a raw token document (used by the publish pipeline
/// and by tenant override application).
pub fn parse_token_document(value: &Value) -> Result<TokenNode, ThemingError> {
    let node: TokenNode = serde_json::from_value(value.clone())
        .map_err(|err| ThemingError::InvalidTokens(err.to_string()))?;
    validate_node(&node, 0)?;
    Ok(node)
}

fn validate_node(node: &TokenNode, depth: usize) -> Result<(), ThemingError> {
    if depth > MAX_DEPTH {
        return Err(ThemingError::InvalidTokens(format!(
            "token document exceeds the maximum depth of {MAX_DEPTH}"
        )));
    }
    match node {
        TokenNode::Token(token) => validate_token_value(token),
        TokenNode::Group(entries) => {
            if entries.is_empty() {
                return Err(ThemingError::InvalidTokens(
                    "token group must not be empty".to_string(),
                ));
            }
            for (key, child) in entries {
                if key.starts_with('$') {
                    return Err(ThemingError::InvalidTokens(format!(
                        "group key {key:?} collides with the DTCG $ namespace"
                    )));
                }
                validate_node(child, depth + 1)?;
            }
            Ok(())
        }
    }
}

/// Validate one leaf: known `$type`, and the `$value` shape must match the
/// declared type.
pub fn validate_token_value(token: &TokenValue) -> Result<(), ThemingError> {
    let Some(kind) = token.r#type.as_deref() else {
        return Err(ThemingError::InvalidTokens(
            "leaf tokens must declare a $type".to_string(),
        ));
    };
    if !KNOWN_TOKEN_TYPES.contains(&kind) {
        return Err(ThemingError::InvalidTokens(format!(
            "unknown token type {kind:?} (known: {KNOWN_TOKEN_TYPES:?})"
        )));
    }
    match kind {
        "color" => {
            let raw = token.value.as_str().ok_or_else(|| {
                ThemingError::InvalidTokens("color $value must be a string".into())
            })?;
            parse_hex_color(raw).ok_or_else(|| {
                ThemingError::InvalidTokens(format!(
                    "color {raw:?} must be #rgb, #rrggbb or #rrggbbaa"
                ))
            })?;
            Ok(())
        }
        "dimension" | "duration" => {
            let raw = token.value.as_str().ok_or_else(|| {
                ThemingError::InvalidTokens(format!("{kind} $value must be a string"))
            })?;
            validate_dimension(raw, kind)
        }
        "fontFamily" => {
            let raw = token.value.as_str().ok_or_else(|| {
                ThemingError::InvalidTokens("fontFamily $value must be a string".into())
            })?;
            if raw.trim().is_empty() || raw.len() > 512 {
                return Err(ThemingError::InvalidTokens(
                    "fontFamily $value must be a non-empty string under 512 bytes".to_string(),
                ));
            }
            Ok(())
        }
        "fontWeight" => {
            let weight = token
                .value
                .as_u64()
                .or_else(|| token.value.as_i64().and_then(|v| u64::try_from(v).ok()))
                .ok_or_else(|| {
                    ThemingError::InvalidTokens("fontWeight $value must be a number".into())
                })?;
            if !(1..=1000).contains(&weight) {
                return Err(ThemingError::InvalidTokens(format!(
                    "fontWeight {weight} must be in 1..=1000"
                )));
            }
            Ok(())
        }
        "number" => {
            if !(token.value.is_number()) {
                return Err(ThemingError::InvalidTokens(
                    "number $value must be a number".to_string(),
                ));
            }
            Ok(())
        }
        "cubicBezier" => {
            let items = token.value.as_array().ok_or_else(|| {
                ThemingError::InvalidTokens("cubicBezier $value must be an array".into())
            })?;
            if items.len() != 4 || !items.iter().all(Value::is_number) {
                return Err(ThemingError::InvalidTokens(
                    "cubicBezier $value must be four numbers".to_string(),
                ));
            }
            Ok(())
        }
        "typography" => {
            if token.value.is_object() {
                Ok(())
            } else {
                Err(ThemingError::InvalidTokens(
                    "typography $value must be an object".to_string(),
                ))
            }
        }
        _ => Ok(()),
    }
}

/// Deep-merge two token documents: groups merge recursively, leaves of the
/// overlay replace the base. The base is not modified.
pub fn merge_tokens(base: &TokenNode, overlay: &TokenNode) -> TokenNode {
    match (base, overlay) {
        (TokenNode::Token(_), overlay) => overlay.clone(),
        (TokenNode::Group(base_entries), TokenNode::Group(overlay_entries)) => {
            let mut merged = base_entries.clone();
            for (key, overlay_child) in overlay_entries {
                match merged.get_mut(key) {
                    Some(existing) => {
                        *existing = merge_tokens(existing, overlay_child);
                    }
                    None => {
                        merged.insert(key.clone(), overlay_child.clone());
                    }
                }
            }
            TokenNode::Group(merged)
        }
        // Group overlay over a leaf base: replace the leaf.
        (TokenNode::Group(_), TokenNode::Token(_)) => overlay.clone(),
    }
}

/// Flatten a token document to dotted paths.
pub fn flatten(node: &TokenNode) -> BTreeMap<String, TokenValue> {
    let mut out = BTreeMap::new();
    flatten_into(node, "", &mut out);
    out
}

fn flatten_into(node: &TokenNode, prefix: &str, out: &mut BTreeMap<String, TokenValue>) {
    match node {
        TokenNode::Token(token) => {
            out.insert(prefix.to_string(), token.clone());
        }
        TokenNode::Group(entries) => {
            for (key, child) in entries {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten_into(child, &path, out);
            }
        }
    }
}

/// Apply tenant overrides (a flat path → token map) to a merged token
/// document, enforcing the manifest's `overridable` allow-list and type
/// agreement with the token being replaced.
pub fn apply_overrides(
    base: &TokenNode,
    overrides: &BTreeMap<String, TokenValue>,
    manifest: &Manifest,
) -> Result<TokenNode, ThemingError> {
    if overrides.is_empty() {
        return Ok(base.clone());
    }
    let flat = flatten(base);
    for (path, token) in overrides {
        if !manifest.allows_override(path) {
            return Err(ThemingError::OverrideNotAllowed { path: path.clone() });
        }
        let Some(existing) = flat.get(path) else {
            return Err(ThemingError::OverrideNotAllowed { path: path.clone() });
        };
        if let (Some(expected), Some(found)) = (existing.r#type.as_deref(), token.r#type.as_deref())
            && expected != found
        {
            return Err(ThemingError::OverrideTypeMismatch {
                path: path.clone(),
                expected: expected.to_string(),
                found: found.to_string(),
            });
        }
        validate_token_value(token)?;
    }

    // Walk a copy, replacing leaves on the override paths.
    let mut result = base.clone();
    for (path, token) in overrides {
        replace_leaf(&mut result, path, token.clone());
    }
    Ok(result)
}

/// Replace the token at `path` inside `node` with `token`. Each segment
/// navigates one group level; the final segment names the leaf entry.
fn replace_leaf(node: &mut TokenNode, path: &str, token: TokenValue) {
    let (head, tail) = match path.split_once('.') {
        Some(pair) => pair,
        None => {
            *node = TokenNode::Token(token);
            return;
        }
    };
    let TokenNode::Group(entries) = node else {
        return;
    };
    let Some(child) = entries.get_mut(head) else {
        return;
    };
    match tail.split_once('.') {
        Some(_) => replace_leaf(child, tail, token),
        None => {
            // The final hop: replace the leaf named `tail` inside `child`.
            if let TokenNode::Group(grandchildren) = child {
                grandchildren.insert(tail.to_string(), TokenNode::Token(token));
            }
        }
    }
}

fn validate_dimension(raw: &str, kind: &str) -> Result<(), ThemingError> {
    // `0` is a valid dimension; otherwise: number + known unit.
    if raw == "0" {
        return Ok(());
    }
    let Some(index) = raw.find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-') else {
        return Err(ThemingError::InvalidTokens(format!(
            "{kind} {raw:?} must be a number followed by a known unit"
        )));
    };
    let (number, unit) = (&raw[..index], &raw[index..]);
    if number.trim().is_empty()
        || number.parse::<f64>().is_err()
        || !KNOWN_UNITS.contains(&unit.trim())
    {
        return Err(ThemingError::InvalidTokens(format!(
            "{kind} {raw:?} uses an unknown unit (known: {KNOWN_UNITS:?})"
        )));
    }
    Ok(())
}

/// Parse `#rgb`, `#rrggbb` or `#rrggbbaa` into `[r, g, b, a]` (0..=255).
pub fn parse_hex_color(raw: &str) -> Option<[u8; 4]> {
    let hex = raw.strip_prefix('#')?;
    let digits: Vec<u8> = hex
        .as_bytes()
        .iter()
        .map(|b| (*b as char).to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    match digits.len() {
        3 => Some([digits[0] * 17, digits[1] * 17, digits[2] * 17, 255]),
        4 => Some([
            digits[0] * 17,
            digits[1] * 17,
            digits[2] * 17,
            digits[3] * 17,
        ]),
        6 => Some([
            digits[0] * 16 + digits[1],
            digits[2] * 16 + digits[3],
            digits[4] * 16 + digits[5],
            255,
        ]),
        8 => Some([
            digits[0] * 16 + digits[1],
            digits[2] * 16 + digits[3],
            digits[4] * 16 + digits[5],
            digits[6] * 16 + digits[7],
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "color": {
                "brand": {
                    "primary": { "$value": "#2563eb", "$type": "color" },
                    "contrast": { "$value": "#ffffff", "$type": "color" }
                },
                "background": {
                    "primary": { "$value": "#ffffff", "$type": "color" }
                }
            },
            "radius": { "md": { "$value": "0.5rem", "$type": "dimension" } },
            "spacing": { "scale": { "$value": 1, "$type": "number" } }
        })
    }

    #[test]
    fn parses_and_flattens_a_document() {
        let tree = parse_token_document(&doc()).unwrap();
        let flat = flatten(&tree);
        assert_eq!(
            flat.get("color.brand.primary").unwrap().value,
            json!("#2563eb")
        );
        assert_eq!(flat.len(), 5);
    }

    #[test]
    fn rejects_unknown_types_and_bad_values() {
        let bad_type = json!({ "color": { "x": { "$value": "1px", "$type": "banana" } } });
        assert!(matches!(
            parse_token_document(&bad_type).unwrap_err(),
            ThemingError::InvalidTokens(_)
        ));

        let bad_color = json!({ "color": { "x": { "$value": "blue", "$type": "color" } } });
        assert!(parse_token_document(&bad_color).is_err());

        let bad_dimension =
            json!({ "radius": { "x": { "$value": "1furlong", "$type": "dimension" } } });
        assert!(parse_token_document(&bad_dimension).is_err());

        let missing_type = json!({ "color": { "x": { "$value": "#fff" } } });
        assert!(parse_token_document(&missing_type).is_err());
    }

    #[test]
    fn merge_replaces_leaves_and_extends_groups() {
        let base = parse_token_document(&doc()).unwrap();
        let overlay = parse_token_document(&json!({
            "color": { "brand": { "primary": { "$value": "#7c3aed", "$type": "color" } } },
            "font": { "size": { "base": { "$value": "1rem", "$type": "dimension" } } }
        }))
        .unwrap();
        let merged = merge_tokens(&base, &overlay);
        let flat = flatten(&merged);
        assert_eq!(
            flat.get("color.brand.primary").unwrap().value,
            json!("#7c3aed")
        );
        assert_eq!(
            flat.get("color.brand.contrast").unwrap().value,
            json!("#ffffff")
        );
        assert_eq!(flat.get("font.size.base").unwrap().value, json!("1rem"));
    }

    #[test]
    fn overrides_respect_the_allowlist_and_types() {
        let manifest = crate::domain::manifest::Manifest::parse(&json!({
            "id": "base",
            "version": "1.0.0",
            "themeApi": "^1.0.0",
            "name": { "en": "Base", "ja": "ベース" },
            "tokens": { "light": "tokens/light.json" },
            "overridable": ["color.brand.*", "color.background.*", "font.family.*", "radius.*", "spacing.scale"],
            "layouts": {}
        }))
        .unwrap();
        let tree = parse_token_document(&doc()).unwrap();

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "color.brand.primary".to_string(),
            TokenValue {
                value: json!("#7c3aed"),
                r#type: Some("color".to_string()),
                description: None,
            },
        );
        let applied = apply_overrides(&tree, &overrides, &manifest).unwrap();

        assert_eq!(
            flatten(&applied).get("color.brand.primary").unwrap().value,
            json!("#7c3aed")
        );

        // Disallowed path.
        let mut bad = BTreeMap::new();
        bad.insert(
            "radius.md".to_string(),
            TokenValue {
                value: json!("9rem"),
                r#type: Some("dimension".to_string()),
                description: None,
            },
        );
        // radius.* is allow-listed in this manifest, so this one passes.
        assert!(apply_overrides(&tree, &bad, &manifest).is_ok());

        let mut bad = BTreeMap::new();
        bad.insert(
            "spacing.scale".to_string(),
            TokenValue {
                value: json!("#ffffff"),
                r#type: Some("color".to_string()),
                description: None,
            },
        );
        assert!(matches!(
            apply_overrides(&tree, &bad, &manifest).unwrap_err(),
            ThemingError::OverrideTypeMismatch { .. }
        ));
    }
}
