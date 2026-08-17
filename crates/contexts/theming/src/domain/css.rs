//! CSS custom-property emission: the flattened token document becomes a
//! ready-to-inline `:root{--…}` block (light) plus a dark block inside
//! `prefers-color-scheme`. Output is deterministic (sorted paths) so the
//! ETag derivation stays stable.

use std::collections::BTreeMap;

use crate::domain::tokens::{TokenNode, TokenValue, flatten};

/// Emit the CSS custom-property blocks for a resolved theme.
pub fn emit_css_vars(light: &TokenNode, dark: Option<&TokenNode>) -> String {
    let mut out = String::new();
    out.push_str(block(&flatten(light)).as_str());
    if let Some(dark) = dark {
        out.push_str("@media (prefers-color-scheme: dark){");
        out.push_str(block(&flatten(dark)).as_str());
        out.push('}');
    }
    out
}

fn block(flat: &BTreeMap<String, TokenValue>) -> String {
    let mut declarations = String::new();
    for (path, token) in flat {
        let Some(value) = value_literal(&token.value) else {
            continue;
        };
        declarations.push_str(&format!("--{}:{value};", path.replace('.', "-")));
    }
    if declarations.is_empty() {
        String::new()
    } else {
        format!(":root{{{declarations}}}")
    }
}

fn value_literal(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(raw) => {
            // Strings carry pre-validated CSS values (colors, dimensions,
            // font stacks); emit as-is.
            Some(raw.clone())
        }
        serde_json::Value::Number(raw) => Some(raw.to_string()),
        serde_json::Value::Bool(raw) => Some(raw.to_string()),
        // Objects (typography) and everything else are not custom-property
        // friendly; the frontend consumes them from the structured token
        // tree instead.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn emits_deterministic_blocks() {
        let light = crate::domain::tokens::parse_token_document(&json!({
            "color": {
                "background": { "primary": { "$value": "#ffffff", "$type": "color" } },
                "brand": { "primary": { "$value": "#2563eb", "$type": "color" } }
            },
            "radius": { "md": { "$value": "0.5rem", "$type": "dimension" } },
            "spacing": { "scale": { "$value": 1, "$type": "number" } }
        }))
        .unwrap();
        let css = emit_css_vars(&light, None);
        assert_eq!(
            css,
            ":root{--color-background-primary:#ffffff;--color-brand-primary:#2563eb;\
             --radius-md:0.5rem;--spacing-scale:1;}"
        );

        let again = emit_css_vars(&light, None);
        assert_eq!(css, again);
    }

    #[test]
    fn wraps_the_dark_block_in_a_media_query() {
        let light = crate::domain::tokens::parse_token_document(&json!({
            "color": { "background": { "primary": { "$value": "#ffffff", "$type": "color" } } }
        }))
        .unwrap();
        let dark = crate::domain::tokens::parse_token_document(&json!({
            "color": { "background": { "primary": { "$value": "#0f1117", "$type": "color" } } }
        }))
        .unwrap();
        let css = emit_css_vars(&light, Some(&dark));
        assert!(css.starts_with(":root{--color-background-primary:#ffffff;}"));
        assert!(css.contains(
            "@media (prefers-color-scheme: dark){:root{--color-background-primary:#0f1117;}}"
        ));
    }
}
