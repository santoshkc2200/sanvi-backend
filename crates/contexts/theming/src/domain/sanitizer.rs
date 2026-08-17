//! The custom-CSS sanitizer.
//!
//! Custom CSS is entitlement-gated (`theming.custom_css`), size-capped and
//! sanitised against closed allow-lists: no at-rules (so no `@import`), no
//! `url()`, no expressions, no escapes, no HTML. Anything the sanitizer
//! cannot positively recognise is rejected — fail closed, because this input
//! renders on every page of a tenant's storefront.
//!
//! The accepted surface: allow-listed elements, class selectors, a closed
//! set of pseudo-classes, descendant/child/sibling combinators, and
//! allow-listed properties with plain values. The output is additionally
//! re-scoped under a wrapper class before delivery.

use crate::domain::error::ThemingError;

/// Elements a tenant stylesheet may target.
pub const ALLOWED_ELEMENTS: &[&str] = &[
    "body",
    "header",
    "main",
    "footer",
    "nav",
    "section",
    "article",
    "aside",
    "div",
    "span",
    "p",
    "a",
    "button",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "ul",
    "ol",
    "li",
    "img",
    "form",
    "input",
    "select",
    "textarea",
    "label",
    "table",
    "thead",
    "tbody",
    "tr",
    "td",
    "th",
    "strong",
    "em",
    "small",
    "blockquote",
    "code",
    "pre",
    "hr",
    "figure",
    "figcaption",
    "picture",
    "details",
    "summary",
    "fieldset",
    "legend",
    "dialog",
];

/// Pseudo-classes that cannot smuggle content or state manipulation.
pub const ALLOWED_PSEUDO_CLASSES: &[&str] = &[
    "hover",
    "focus",
    "active",
    "focus-within",
    "focus-visible",
    "visited",
    "disabled",
    "enabled",
    "first-child",
    "last-child",
    "only-child",
    "nth-child",
    "first-of-type",
    "last-of-type",
    "empty",
    "checked",
];

/// Pseudo-elements (no `content` property is accepted, which defuses the
/// content-injection vector they would otherwise open).
pub const ALLOWED_PSEUDO_ELEMENTS: &[&str] = &["before", "after", "placeholder", "marker"];

/// Properties a tenant stylesheet may set.
pub const ALLOWED_PROPERTIES: &[&str] = &[
    "color",
    "background",
    "background-color",
    "background-position",
    "background-size",
    "background-repeat",
    "border",
    "border-top",
    "border-right",
    "border-bottom",
    "border-left",
    "border-width",
    "border-color",
    "border-style",
    "border-radius",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "box-shadow",
    "outline",
    "outline-color",
    "outline-width",
    "opacity",
    "font-family",
    "font-size",
    "font-weight",
    "font-style",
    "line-height",
    "letter-spacing",
    "text-align",
    "text-decoration",
    "text-decoration-color",
    "text-transform",
    "text-indent",
    "text-shadow",
    "text-overflow",
    "white-space",
    "word-break",
    "overflow-wrap",
    "margin",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "padding",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "width",
    "height",
    "min-width",
    "min-height",
    "max-width",
    "max-height",
    "display",
    "position",
    "top",
    "right",
    "bottom",
    "left",
    "inset",
    "flex",
    "flex-direction",
    "flex-wrap",
    "flex-grow",
    "flex-shrink",
    "flex-basis",
    "align-items",
    "align-self",
    "justify-content",
    "justify-items",
    "gap",
    "row-gap",
    "column-gap",
    "grid-template-columns",
    "order",
    "z-index",
    "overflow",
    "overflow-x",
    "overflow-y",
    "visibility",
    "cursor",
    "transition",
    "transition-duration",
    "transform",
    "transform-origin",
    "box-sizing",
    "list-style",
    "list-style-type",
    "vertical-align",
];

/// Validated, sanitised custom CSS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedCss {
    css: String,
}

impl SanitizedCss {
    /// Build from already-validated CSS (e.g. what is stored after the
    /// write-time sanitisation gate).
    pub fn new(css: String) -> Self {
        Self { css }
    }

    pub fn as_str(&self) -> &str {
        &self.css
    }

    /// Re-scope every rule under a wrapper class, so tenant CSS cannot
    /// escape its own storefront container.
    pub fn scoped(&self, scope_class: &str) -> String {
        scope_css(&self.css, scope_class)
    }
}

/// Re-scope raw (already validated) CSS under a wrapper class.
pub fn scope_css(css: &str, scope_class: &str) -> String {
    let mut out = String::with_capacity(css.len() + css.len() / 4);
    for block in css.split('}') {
        let Some((selector_part, declarations)) = block.split_once('{') else {
            continue;
        };
        let selectors = selector_part.split(',').map(str::trim);
        let mut first = true;
        for selector in selectors {
            if selector.is_empty() {
                continue;
            }
            if !first {
                out.push_str(", ");
            }
            first = false;
            out.push_str(scope_class);
            out.push(' ');
            out.push_str(selector);
        }
        if first {
            continue;
        }
        out.push_str(" { ");
        out.push_str(declarations.trim());
        out.push_str(" } ");
    }
    out.trim_end().to_string()
}

/// Sanitize a raw stylesheet. Returns the sanitised CSS or the first
/// violation.
pub fn sanitize_css(input: &str, max_bytes: usize) -> Result<SanitizedCss, ThemingError> {
    let detail = |msg: String| ThemingError::InvalidCustomCss(msg);

    if input.len() > max_bytes {
        return Err(detail(format!(
            "stylesheet is {} bytes, over the {max_bytes} byte cap",
            input.len()
        )));
    }

    let stripped = strip_comments(input)?;

    // Hard rejections: anything that could smuggle imports, URLs, scripts,
    // HTML or escapes.
    for forbidden in [
        "@",
        "url(",
        "expression(",
        "javascript:",
        "vbscript:",
        "data:",
        "behavior:",
        "-moz-binding",
        "<",
        "\\",
        "progid:",
        "/*",
        "*/",
    ] {
        if stripped.contains(forbidden) {
            return Err(detail(format!("forbidden construct {forbidden:?}")));
        }
    }

    if stripped.trim().is_empty() {
        return Ok(SanitizedCss { css: String::new() });
    }

    // Sequential brace-aware scan: depth 0 collects selectors, depth 1
    // collects declarations. Nested or stray braces fail closed — they
    // cannot be produced by the accepted grammar.
    let mut declarations_seen = 0;
    let mut depth: i64 = 0;
    let mut buffer = String::new();
    for ch in stripped.chars() {
        match ch {
            '{' if depth == 0 => {
                validate_selectors(&buffer)?;
                buffer.clear();
                depth = 1;
            }
            '{' => {
                return Err(detail("nested blocks are not allowed".to_string()));
            }
            '}' if depth == 1 => {
                validate_declarations(&buffer)?;
                declarations_seen += buffer
                    .split(';')
                    .filter(|part| !part.trim().is_empty())
                    .count();
                buffer.clear();
                depth = 0;
            }
            '}' => {
                return Err(detail("unbalanced braces".to_string()));
            }
            _ => buffer.push(ch),
        }
    }
    if depth != 0 || !buffer.trim().is_empty() {
        return Err(detail("unbalanced braces".to_string()));
    }
    if declarations_seen == 0 {
        return Err(detail("stylesheet contains no rules".to_string()));
    }

    Ok(SanitizedCss { css: stripped })
}

fn strip_comments(input: &str) -> Result<String, ThemingError> {
    // Comments are stripped (never nested); an unterminated comment fails.
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("*/") else {
            return Err(ThemingError::InvalidCustomCss(
                "unterminated comment".to_string(),
            ));
        };
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn validate_selectors(selectors: &str) -> Result<(), ThemingError> {
    let detail = |msg: String| ThemingError::InvalidCustomCss(msg);
    for raw in selectors.split(',') {
        let selector = raw.trim();
        if selector.is_empty() {
            return Err(detail("empty selector".to_string()));
        }
        validate_selector(selector)?;
    }
    Ok(())
}

fn validate_selector(selector: &str) -> Result<(), ThemingError> {
    // tokenise: identifiers, `.class`, `#id`, `:`pseudo, `::`pseudo,
    // combinators (space, `>`, `+`, `~`), `*`.
    let mut tokens = Vec::new();
    let mut chars = selector.chars().peekable();
    let mut current = String::new();
    let mut in_class = false;
    let mut in_id = false;
    let mut in_pseudo = false;

    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' => {
                push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;
                if !tokens.is_empty() && !matches!(tokens.last(), Some(Token::Combinator(_))) {
                    tokens.push(Token::Combinator(" "));
                }
            }
            '>' | '+' | '~' => {
                push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;
                if matches!(tokens.last(), Some(Token::Combinator(" "))) {
                    tokens.pop();
                }
                let combo = match ch {
                    '>' => ">",
                    '+' => "+",
                    _ => "~",
                };
                tokens.push(Token::Combinator(combo));
            }
            '.' => {
                push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;
                in_class = true;
                current.push('.');
            }
            '#' => {
                push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;
                in_id = true;
                current.push('#');
            }
            ':' => {
                if chars.peek() == Some(&':') {
                    push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;
                    chars.next();
                    in_pseudo = true;
                    current.push_str("::");
                } else {
                    push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;
                    in_pseudo = true;
                    current.push(':');
                }
            }
            '*' => {
                push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;
                tokens.push(Token::Universal);
            }
            '(' | ')' => {
                // Only allow balanced parens on the pseudo with an argument.
                current.push(ch);
            }
            _ if ch.is_alphanumeric() || ch == '-' || ch == '_' => {
                current.push(ch);
            }
            other => {
                return Err(ThemingError::InvalidCustomCss(format!(
                    "unexpected character {other:?} in selector"
                )));
            }
        }
    }
    push_token(&mut tokens, &mut current, in_class, in_id, in_pseudo)?;

    if tokens.is_empty() {
        return Err(ThemingError::InvalidCustomCss("empty selector".to_string()));
    }

    // Validate the token stream: no two combinators in a row, no leading
    // combinator, pseudo argument shape for nth-*.
    let mut last_was_combinator = true;
    for token in &tokens {
        match token {
            Token::Combinator(_) => {
                if last_was_combinator {
                    return Err(ThemingError::InvalidCustomCss(
                        "double or leading combinator".to_string(),
                    ));
                }
                last_was_combinator = true;
            }
            _ => last_was_combinator = false,
        }
    }
    if last_was_combinator {
        return Err(ThemingError::InvalidCustomCss(
            "trailing combinator".to_string(),
        ));
    }
    Ok(())
}

enum Token {
    Combinator(&'static str),
    Universal,
    // Any simple selector (element/class/id/pseudo) — validated in push.
    Simple,
}

fn push_token(
    tokens: &mut Vec<Token>,
    current: &mut String,
    in_class: bool,
    in_id: bool,
    in_pseudo: bool,
) -> Result<(), ThemingError> {
    if current.is_empty() {
        return Ok(());
    }
    let raw = current.trim().to_string();
    current.clear();
    if in_class {
        validate_identifier(&raw[1..], "class")?;
    } else if in_id {
        validate_identifier(&raw[1..], "id")?;
    } else if in_pseudo {
        validate_pseudo(&raw)?;
    } else {
        validate_element(&raw)?;
    }
    tokens.push(Token::Simple);
    Ok(())
}

fn validate_identifier(raw: &str, kind: &str) -> Result<(), ThemingError> {
    let valid = !raw.is_empty()
        && raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(ThemingError::InvalidCustomCss(format!(
            "invalid {kind} name {raw:?}"
        )))
    }
}

fn validate_element(raw: &str) -> Result<(), ThemingError> {
    if ALLOWED_ELEMENTS.contains(&raw) {
        Ok(())
    } else {
        Err(ThemingError::InvalidCustomCss(format!(
            "element {raw:?} is not in the allow-list"
        )))
    }
}

fn validate_pseudo(raw: &str) -> Result<(), ThemingError> {
    let (name, argument) = match raw.split_once('(') {
        Some((name, argument)) => {
            let argument = argument.trim();
            if !argument.ends_with(')') {
                return Err(ThemingError::InvalidCustomCss(
                    "unbalanced pseudo-class argument".to_string(),
                ));
            }
            (name, Some(&argument[..argument.len() - 1]))
        }
        None => (raw, None),
    };
    if let Some(element) = name.strip_prefix("::") {
        if ALLOWED_PSEUDO_ELEMENTS.contains(&element) && argument.is_none() {
            return Ok(());
        }
        return Err(ThemingError::InvalidCustomCss(format!(
            "pseudo-element {name:?} is not allowed"
        )));
    }
    let Some(class) = name.strip_prefix(':') else {
        return Err(ThemingError::InvalidCustomCss(format!(
            "malformed pseudo {raw:?}"
        )));
    };
    if !ALLOWED_PSEUDO_CLASSES.contains(&class) {
        return Err(ThemingError::InvalidCustomCss(format!(
            "pseudo-class :{class} is not in the allow-list"
        )));
    }
    match argument {
        None => Ok(()),
        Some(argument) if matches!(class, "nth-child" | "first-of-type" | "last-of-type") => {
            if valid_anb(argument) {
                Ok(())
            } else {
                Err(ThemingError::InvalidCustomCss(format!(
                    "unsupported pseudo-class argument {argument:?}"
                )))
            }
        }
        Some(_) => Err(ThemingError::InvalidCustomCss(format!(
            "pseudo-class :{class} does not take an argument"
        ))),
    }
}

/// CSS `an+b` micro-syntax for nth-* arguments (plus `odd`/`even`).
fn valid_anb(raw: &str) -> bool {
    let s = raw.trim().replace(' ', "");
    if s.is_empty() || s.len() > 16 {
        return false;
    }
    if s == "odd" || s == "even" {
        return true;
    }
    let Some(n_index) = s.find(['n', 'N']) else {
        return s.parse::<i32>().is_ok();
    };
    let a = &s[..n_index];
    let b = &s[n_index + 1..];
    let a_ok = a.is_empty() || a == "-" || a.parse::<i32>().is_ok();
    let b_ok = {
        let b = b.strip_prefix('+').unwrap_or(b);
        b.is_empty() || b.parse::<i32>().is_ok()
    };
    a_ok && b_ok && !(a.is_empty() && b.is_empty())
}

fn validate_declarations(declarations: &str) -> Result<(), ThemingError> {
    let mut count = 0;
    for part in declarations.split(';') {
        let declaration = part.trim();
        if declaration.is_empty() {
            continue;
        }
        count += 1;
        let Some((property, value)) = declaration.split_once(':') else {
            return Err(ThemingError::InvalidCustomCss(format!(
                "declaration {declaration:?} is missing ':'"
            )));
        };
        let property = property.trim().to_ascii_lowercase();
        if !ALLOWED_PROPERTIES.contains(&property.as_str()) {
            return Err(ThemingError::InvalidCustomCss(format!(
                "property {property:?} is not in the allow-list"
            )));
        }
        validate_value(value.trim())?;
    }
    if count == 0 {
        return Err(ThemingError::InvalidCustomCss(
            "rule has no declarations".to_string(),
        ));
    }
    Ok(())
}

fn validate_value(value: &str) -> Result<(), ThemingError> {
    if value.is_empty() || value.len() > 512 {
        return Err(ThemingError::InvalidCustomCss(
            "declaration value must be 1..=512 bytes".to_string(),
        ));
    }
    for forbidden in ["{", "}", ":", "url(", "expression(", "javascript:", "@"] {
        if value.contains(forbidden) {
            return Err(ThemingError::InvalidCustomCss(format!(
                "value contains forbidden construct {forbidden:?}"
            )));
        }
    }
    // Parentheses and quotes must balance; strings may not contain ';' or
    // braces (they would break the naive split and are rejected above).
    let mut parens = 0i64;
    let mut in_string: Option<char> = None;
    for ch in value.chars() {
        match ch {
            '"' | '\'' => match in_string {
                None => in_string = Some(ch),
                Some(quote) if quote == ch => in_string = None,
                Some(_) => {}
            },
            '(' if in_string.is_none() => parens += 1,
            ')' if in_string.is_none() => parens -= 1,
            _ => {}
        }
        if parens < 0 {
            return Err(ThemingError::InvalidCustomCss(
                "unbalanced parentheses in value".to_string(),
            ));
        }
    }
    if parens != 0 || in_string.is_some() {
        return Err(ThemingError::InvalidCustomCss(
            "unbalanced parentheses or quotes in value".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_benign_rules() {
        let css = r#"
            .hero { background-color: #f6f7f9; padding: 1rem; border-radius: 0.5rem; }
            .hero h2 { color: #1a1d23; font-weight: 700; }
            a.button:hover { background-color: #1d4ed8; }
            .grid > div:first-child { margin-left: 0; }
            li:nth-child(2n) { background: #fff; }
        "#;
        let sanitized = sanitize_css(css, 8192).unwrap();
        let scoped = sanitized.scoped(".sanvi-scope");
        assert!(scoped.starts_with(".sanvi-scope .hero"));
        assert!(scoped.contains(".sanvi-scope a.button:hover"));
    }

    #[test]
    fn rejects_xss_corpus() {
        for payload in [
            "<script>alert(1)</script>",
            "body { background: url(javascript:alert(1)); }",
            "@import url(https://evil.example/x.css);",
            "body { background: url(https://evil.example/x.png); }",
            "body { width: expression(alert(1)); }",
            "body { -moz-binding: url(http://evil/xbl.xml); }",
            "body { behavior: url(#default#time2); }",
            "body { background-image: url(data:text/html;base64,PHNjcmlwdD4=); }",
            "body { color: red\\0A }",
            "</style><script>alert(1)</script><style>",
            "body { font-size: 1rem; } } body { color: red",
            "script { color: red; }",
            "body { content: 'hello'; }",
            "body { custom-property: 1; }",
            "a[href=\"https://evil.example\"] { color: red; }",
            "body::before { content: 'phish'; }",
            "body { color: rgb(1,2; background: red; }",
        ] {
            let result = sanitize_css(payload, 8192);
            assert!(result.is_err(), "payload should be rejected: {payload}");
        }
    }

    #[test]
    fn enforces_the_size_cap() {
        let css = ".a { padding: 1px; }";
        assert!(sanitize_css(css, css.len() - 1).is_err());
        assert!(sanitize_css(css, css.len()).is_ok());
    }

    #[test]
    fn strips_comments() {
        let sanitized = sanitize_css("/* note */ .a { color: red; /* inline */ }", 8192).unwrap();
        assert!(!sanitized.as_str().contains("note"));
        assert!(sanitize_css(".a { color: red; } /* unterminated", 8192).is_err());
    }

    #[test]
    fn rejects_empty_and_ruleless_input() {
        assert!(sanitize_css("", 8192).is_ok());
        assert!(sanitize_css("   ", 8192).is_ok());
        assert!(sanitize_css(".a { }", 8192).is_err());
        assert!(sanitize_css(".a { color: red", 8192).is_err());
    }
}
