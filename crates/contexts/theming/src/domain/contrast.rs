//! WCAG contrast validation over semantic token pairs.
//!
//! Publishing fails when any defined foreground/background pair falls below
//! its ratio threshold (WCAG AA: 4.5:1 for text, 3:1 for large text/UI
//! components). Only pairs whose tokens exist are checked; the pairs are
//! semantic, so a theme cannot dodge the gate by renaming tokens.

use std::collections::BTreeMap;

use crate::domain::error::ThemingError;
use crate::domain::tokens::{TokenNode, TokenValue, flatten, parse_hex_color};

/// A checked pair: (foreground, background, minimum ratio).
pub const CONTRAST_PAIRS: &[(&str, &str, f64)] = &[
    ("color.text.primary", "color.background.primary", 4.5),
    ("color.text.secondary", "color.background.primary", 4.5),
    ("color.text.inverse", "color.background.inverse", 4.5),
    ("color.brand.contrast", "color.brand.primary", 4.5),
    ("color.brand.contrast", "color.brand.hover", 4.5),
    ("color.focus.ring", "color.background.primary", 3.0),
];

/// One failed pair.
#[derive(Debug, Clone, PartialEq)]
pub struct ContrastViolation {
    pub foreground: String,
    pub background: String,
    pub foreground_value: String,
    pub background_value: String,
    pub ratio: f64,
    pub minimum: f64,
}

/// Check one mode's token set; returns every violation.
pub fn check_mode(flat: &BTreeMap<String, TokenValue>) -> Vec<ContrastViolation> {
    let mut violations = Vec::new();
    for (foreground, background, minimum) in CONTRAST_PAIRS {
        let (Some(fg), Some(bg)) = (flat.get(*foreground), flat.get(*background)) else {
            continue;
        };
        let (Some(fg_value), Some(bg_value)) = (fg.value.as_str(), bg.value.as_str()) else {
            continue;
        };
        let (Some(fg_color), Some(bg_color)) =
            (parse_hex_color(fg_value), parse_hex_color(bg_value))
        else {
            continue;
        };
        let ratio = contrast_ratio(fg_color, bg_color);
        if ratio < *minimum {
            violations.push(ContrastViolation {
                foreground: (*foreground).to_string(),
                background: (*background).to_string(),
                foreground_value: fg_value.to_string(),
                background_value: bg_value.to_string(),
                ratio,
                minimum: *minimum,
            });
        }
    }
    violations
}

/// Check both modes of a theme. `dark` is `None` when the theme has no dark
/// tokens. Returns `Err` when any pair fails — publishing is refused unless
/// the caller explicitly allows warnings.
pub fn check_theme(light: &TokenNode, dark: Option<&TokenNode>) -> Result<(), ThemingError> {
    let mut problems = Vec::new();
    problems.extend(format_violations("light", &check_mode(&flatten(light))));
    if let Some(dark) = dark {
        problems.extend(format_violations("dark", &check_mode(&flatten(dark))));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ThemingError::ContrastFailure(problems.join("; ")))
    }
}

fn format_violations(mode: &str, violations: &[ContrastViolation]) -> Vec<String> {
    violations
        .iter()
        .map(|violation| {
            format!(
                "{mode}: {} ({}) on {} ({}) is {:.2}:1, below {:.2}:1",
                violation.foreground,
                violation.foreground_value,
                violation.background,
                violation.background_value,
                violation.ratio,
                violation.minimum
            )
        })
        .collect()
}

/// WCAG relative luminance and contrast ratio (sRGB, with alpha flattened
/// against white — tokens are opaque in practice).
pub fn relative_luminance(rgb: [u8; 4]) -> f64 {
    let alpha = rgb[3] as f64 / 255.0;
    let channel = |byte: u8| {
        // Flatten against white before converting sRGB to linear light.
        let c = (byte as f64 / 255.0) * alpha + (1.0 - alpha);
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

/// Contrast ratio between two colors (1..=21).
pub fn contrast_ratio(foreground: [u8; 4], background: [u8; 4]) -> f64 {
    let l1 = relative_luminance(foreground);
    let l2 = relative_luminance(background);
    let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tokens(json: serde_json::Value) -> TokenNode {
        crate::domain::tokens::parse_token_document(&json).unwrap()
    }

    #[test]
    fn ratio_extremes() {
        // Black on white is the maximum ratio.
        let black = parse_hex_color("#000000").unwrap();
        let white = parse_hex_color("#ffffff").unwrap();
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(white, white) - 1.0).abs() < 0.01);

        // Fully transparent black is white after compositing, not black.
        assert!((contrast_ratio([0, 0, 0, 0], white) - 1.0).abs() < 0.01);
    }

    #[test]
    fn known_good_palette_passes() {
        let light = tokens(json!({
            "color": {
                "text": {
                    "primary": { "$value": "#1a1d23", "$type": "color" },
                    "secondary": { "$value": "#4a505c", "$type": "color" },
                    "inverse": { "$value": "#ffffff", "$type": "color" }
                },
                "background": {
                    "primary": { "$value": "#ffffff", "$type": "color" },
                    "inverse": { "$value": "#1a1d23", "$type": "color" }
                },
                "brand": {
                    "primary": { "$value": "#2563eb", "$type": "color" },
                    "hover": { "$value": "#1d4ed8", "$type": "color" },
                    "contrast": { "$value": "#ffffff", "$type": "color" }
                },
                "focus": { "ring": { "$value": "#1d4ed8", "$type": "color" } }
            }
        }));
        check_theme(&light, None).unwrap();
    }

    #[test]
    fn known_bad_palette_fails() {
        let light = tokens(json!({
            "color": {
                "text": {
                    "primary": { "$value": "#94a3b8", "$type": "color" },
                    "secondary": { "$value": "#cbd5e1", "$type": "color" },
                    "inverse": { "$value": "#ffffff", "$type": "color" }
                },
                "background": {
                    "primary": { "$value": "#ffffff", "$type": "color" },
                    "inverse": { "$value": "#1a1d23", "$type": "color" }
                },
                "brand": {
                    "primary": { "$value": "#fde047", "$type": "color" },
                    "hover": { "$value": "#fde047", "$type": "color" },
                    "contrast": { "$value": "#ffffff", "$type": "color" }
                },
                "focus": { "ring": { "$value": "#ffffff", "$type": "color" } }
            }
        }));
        let err = check_theme(&light, None).unwrap_err();
        let detail = err.to_string();
        assert!(detail.contains("color.text.primary"), "{detail}");
        assert!(detail.contains("color.brand.contrast"), "{detail}");
    }

    #[test]
    fn missing_pairs_are_skipped_not_failed() {
        let sparse = tokens(json!({
            "color": {
                "brand": {
                    "primary": { "$value": "#2563eb", "$type": "color" },
                    "contrast": { "$value": "#ffffff", "$type": "color" }
                }
            }
        }));
        check_theme(&sparse, None).unwrap();
    }
}
