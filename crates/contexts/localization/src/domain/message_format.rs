//! A deliberately small ICU MessageFormat engine.
//!
//! Supports the subset the backend catalogs use — plain substitution,
//! `number`/`date`/`time` formatting, `plural` (with exact matches,
//! `offset:` and `#` substitution) and `select`. Enough for English plurals
//! and Japanese counters with one syntax, with no external parser.
//!
//! Quoting follows ICU's apostrophe rules: `''` is a literal apostrophe.
//! Unbalanced braces are rejected at parse time (`cargo i18n check` gates
//! this in CI).
//!
//! Missing arguments render as the placeholder name in braces (never an
//! empty string) so a formatting bug is visible in the output instead of
//! silently dropping content.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::domain::LocalizationError;
use crate::domain::formatting::FormattingPolicy;

/// One message argument. Conversions from `&str`/`String` (text), integers
/// and floats (numbers) and `DateTime<Utc>` (dates) keep call sites terse.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Text(String),
    Number(f64),
    DateTime(DateTime<Utc>),
}

impl From<&str> for Arg {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for Arg {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<i64> for Arg {
    fn from(value: i64) -> Self {
        Self::Number(value as f64)
    }
}

impl From<i32> for Arg {
    fn from(value: i32) -> Self {
        Self::Number(f64::from(value))
    }
}

impl From<u32> for Arg {
    fn from(value: u32) -> Self {
        Self::Number(f64::from(value))
    }
}

impl From<f64> for Arg {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<DateTime<Utc>> for Arg {
    fn from(value: DateTime<Utc>) -> Self {
        Self::DateTime(value)
    }
}

/// A pre-parsed message pattern: a sequence of literal text and
/// placeholders.
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    Text(String),
    /// `{name}` or `{name, number}` / `{name, date}` / `{name, time}`.
    Substitution {
        name: String,
        style: ArgStyle,
    },
    /// The `#` shadow inside a plural body: replaced with the formatted
    /// selector number.
    NumberShadow,
    /// `{name, plural, ...}` / `{name, select, ...}`.
    Branch {
        name: String,
        kind: BranchKind,
        options: Vec<(String, Vec<Segment>)>,
        offset: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgStyle {
    Raw,
    Number,
    Integer,
    Date,
    Time,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchKind {
    Plural,
    Select,
}

/// Parse a pattern into segments. Used both at render time (via the
/// catalog's cached parse) and by `cargo i18n check` to validate catalogs.
pub fn parse_pattern(pattern: &str) -> Result<Vec<Segment>, LocalizationError> {
    let mut parser = Parser {
        source: pattern,
        pos: 0,
        key: String::new(),
    };
    parser.parse_segments(false)
}

pub(crate) fn parse_pattern_with_key(
    key: &str,
    pattern: &str,
) -> Result<Vec<Segment>, LocalizationError> {
    let mut parser = Parser {
        source: pattern,
        pos: 0,
        key: key.to_string(),
    };
    parser.parse_segments(false)
}

fn err(key: &str, detail: impl Into<String>) -> LocalizationError {
    LocalizationError::InvalidPattern {
        key: key.to_string(),
        detail: detail.into(),
    }
}

struct Parser<'a> {
    source: &'a str,
    pos: usize,
    key: String,
}

impl<'a> Parser<'a> {
    fn parse_segments(&mut self, in_brace: bool) -> Result<Vec<Segment>, LocalizationError> {
        let mut segments = Vec::new();
        let mut text = String::new();
        let bytes = self.source.as_bytes();

        while self.pos < bytes.len() {
            let ch = self.source[self.pos..].chars().next().unwrap_or('\0');
            match ch {
                '\'' => {
                    // ICU apostrophe quoting: '' is a literal '; otherwise
                    // the next run of text up to the closing ' is literal.
                    if self.source[self.pos..].starts_with("''") {
                        text.push('\'');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        while self.pos < bytes.len() {
                            if self.source[self.pos..].starts_with('\'') {
                                self.pos += 1;
                                break;
                            }
                            let c = self.source[self.pos..].chars().next().unwrap_or('\0');
                            text.push(c);
                            self.pos += c.len_utf8();
                        }
                    }
                }
                '{' if !in_brace && self.source[self.pos..].starts_with("'{'") => {
                    text.push('{');
                    self.pos += 3;
                }
                '{' => {
                    if !text.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    self.pos += 1;
                    segments.push(self.parse_placeholder()?);
                }
                '#' if in_brace => {
                    // The plural shadow: only special inside branch bodies.
                    if !text.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    self.pos += 1;
                    segments.push(Segment::Substitution {
                        name: "#".to_string(),
                        style: ArgStyle::Integer,
                    });
                }
                '#' => {
                    text.push('#');
                    self.pos += 1;
                }
                '}' if in_brace => {
                    if !text.is_empty() {
                        segments.push(Segment::Text(std::mem::take(&mut text)));
                    }
                    self.pos += 1;
                    return Ok(segments);
                }
                '}' => {
                    // A bare closing brace is literal in ICU's grammar
                    // unless it closes a placeholder.
                    text.push('}');
                    self.pos += 1;
                }
                _ => {
                    text.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
        if in_brace {
            return Err(err(&self.key, "unterminated placeholder"));
        }
        if !text.is_empty() {
            segments.push(Segment::Text(text));
        }
        Ok(segments)
    }

    fn parse_placeholder(&mut self) -> Result<Segment, LocalizationError> {
        let name = self.parse_name()?;
        if self.source[self.pos..].starts_with('}') {
            self.pos += 1;
            return Ok(Segment::Substitution {
                name,
                style: ArgStyle::Raw,
            });
        }
        if !self.source[self.pos..].starts_with(',') {
            return Err(err(
                &self.key,
                format!("expected ',' or '}}' after {{{name}"),
            ));
        }
        self.pos += 1;
        let kind = self.parse_name()?;
        match kind.as_str() {
            "number" | "integer" | "date" | "time" => {
                let style = match kind.as_str() {
                    "number" => ArgStyle::Number,
                    "integer" => ArgStyle::Integer,
                    "date" => ArgStyle::Date,
                    "time" => ArgStyle::Time,
                    _ => unreachable!(),
                };
                self.expect('}')?;
                Ok(Segment::Substitution { name, style })
            }
            "plural" | "select" => {
                let branch = if kind == "plural" {
                    BranchKind::Plural
                } else {
                    BranchKind::Select
                };
                self.parse_branch(name, branch)
            }
            other => Err(err(
                &self.key,
                format!("unsupported argument type {other:?} in {{{name}}}"),
            )),
        }
    }

    fn parse_branch(
        &mut self,
        name: String,
        kind: BranchKind,
    ) -> Result<Segment, LocalizationError> {
        // `plural, ...` / `select, ...` — the comma separates the type from
        // the option list.
        self.expect(',')?;
        let mut offset = 0;
        // Optional `offset:N` before the option list (plural only; ICU
        // places it after the comma, with no trailing comma of its own).
        if kind == BranchKind::Plural {
            self.skip_whitespace();
            let rest = &self.source[self.pos..];
            if let Some(_rest) = rest.strip_prefix("offset:") {
                self.pos += "offset:".len();
                let value = self.parse_number()?;
                offset = value;
            }
        }
        let mut options = Vec::new();
        loop {
            self.skip_whitespace();
            if self.source[self.pos..].starts_with('}') {
                self.pos += 1;
                break;
            }
            let selector = self.parse_name()?;
            self.expect('{')?;
            let body = self.parse_segments(true)?;
            // Inside a plural body, the `#` substitution becomes the
            // number shadow.
            let body = body
                .into_iter()
                .map(|segment| match segment {
                    Segment::Substitution { name, style } if name == "#" => {
                        let _ = style;
                        Segment::NumberShadow
                    }
                    other => other,
                })
                .collect();
            options.push((selector, body));
            self.skip_whitespace();
        }
        if options.is_empty() {
            return Err(err(&self.key, format!("{{{name}}} has no options")));
        }
        if !options.iter().any(|(selector, _)| selector == "other") {
            return Err(err(
                &self.key,
                format!("{{{name}}} must define an `other` option"),
            ));
        }
        Ok(Segment::Branch {
            name,
            kind,
            options,
            offset,
        })
    }

    fn parse_name(&mut self) -> Result<String, LocalizationError> {
        self.skip_whitespace();
        let start = self.pos;
        let bytes = self.source.as_bytes();
        // Selectors may start with '=' (exact-value matches like `=0`).
        if self.source[self.pos..].starts_with('=') {
            self.pos += 1;
        }
        while self.pos < bytes.len() {
            let ch = self.source[self.pos..].chars().next().unwrap_or('\0');
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(err(&self.key, "expected an identifier"));
        }
        Ok(self.source[start..self.pos].to_string())
    }

    fn parse_number(&mut self) -> Result<i64, LocalizationError> {
        self.skip_whitespace();
        let start = self.pos;
        let bytes = self.source.as_bytes();
        while self.pos < bytes.len()
            && self.source[self.pos..].starts_with(|c: char| c.is_ascii_digit())
        {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(err(&self.key, "expected a number"));
        }
        self.source[start..self.pos]
            .parse::<i64>()
            .map_err(|_| err(&self.key, "number out of range"))
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.source.len() && self.source[self.pos..].starts_with(' ') {
            self.pos += 1;
        }
    }

    fn expect(&mut self, ch: char) -> Result<(), LocalizationError> {
        self.skip_whitespace();
        if self.source[self.pos..].starts_with(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(err(
                &self.key,
                format!("expected {ch:?} at position {}", self.pos),
            ))
        }
    }
}

/// Collect the placeholder names a pattern references. `cargo i18n check`
/// uses this to assert translations reference exactly the same arguments
/// as their source pattern.
pub fn placeholder_names(segments: &[Segment]) -> std::collections::BTreeSet<String> {
    fn walk(segments: &[Segment], out: &mut std::collections::BTreeSet<String>) {
        for segment in segments {
            match segment {
                Segment::Substitution { name, .. } => {
                    out.insert(name.clone());
                }
                Segment::Branch { name, options, .. } => {
                    out.insert(name.clone());
                    for (_, body) in options {
                        walk(body, out);
                    }
                }
                Segment::Text(_) | Segment::NumberShadow => {}
            }
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(segments, &mut out);
    out
}

/// Render a parsed pattern with the given arguments.
pub fn render(
    segments: &[Segment],
    args: &HashMap<String, Arg>,
    policy: &FormattingPolicy,
) -> String {
    let mut out = String::new();
    render_segments(segments, args, policy, &mut out, None);
    out
}

fn render_segments(
    segments: &[Segment],
    args: &HashMap<String, Arg>,
    policy: &FormattingPolicy,
    out: &mut String,
    number_shadow: Option<f64>,
) {
    for segment in segments {
        match segment {
            Segment::Text(text) => out.push_str(text),
            Segment::NumberShadow => match number_shadow {
                Some(n) => out.push_str(&policy.format_number(n)),
                None => out.push('#'),
            },
            Segment::Substitution { name, style } => {
                match args.get(name) {
                    Some(Arg::Text(text)) => out.push_str(text),
                    Some(Arg::Number(n)) => {
                        let formatted = match style {
                            ArgStyle::Raw | ArgStyle::Number => policy.format_number(*n),
                            ArgStyle::Integer => policy.format_number_integer(*n as i64),
                            ArgStyle::Date | ArgStyle::Time => policy.format_number(*n),
                        };
                        out.push_str(&formatted);
                    }
                    Some(Arg::DateTime(dt)) => match style {
                        ArgStyle::Date => out.push_str(&policy.format_date(*dt)),
                        ArgStyle::Time => out.push_str(&policy.format_time(*dt)),
                        ArgStyle::Raw | ArgStyle::Number | ArgStyle::Integer => {
                            out.push_str(&dt.to_rfc3339())
                        }
                    },
                    None => {
                        // Never silent: render the missing placeholder so a
                        // wiring bug is visible in the output.
                        out.push('{');
                        out.push_str(name);
                        out.push('}');
                    }
                }
            }
            Segment::Branch {
                name,
                kind,
                options,
                offset,
            } => {
                let (selector, shadow) = match kind {
                    BranchKind::Plural => {
                        let n = match args.get(name) {
                            Some(Arg::Number(n)) => *n,
                            Some(Arg::Text(text)) => text.parse::<f64>().unwrap_or(0.0),
                            _ => 0.0,
                        };
                        // Exact-value selectors (`=0`, `=2`) beat the
                        // grammatical category.
                        let exact = if n.fract() == 0.0 {
                            Some(format!("={}", n as i64))
                        } else {
                            None
                        };
                        let selector = match exact {
                            Some(exact) if options.iter().any(|(option, _)| option == &exact) => {
                                exact
                            }
                            _ => plural_selector(n - *offset as f64),
                        };
                        (selector, Some(n))
                    }
                    BranchKind::Select => {
                        let selector = match args.get(name) {
                            Some(Arg::Text(text)) => text.clone(),
                            Some(Arg::Number(n)) => policy.format_number_integer(*n as i64),
                            _ => String::from("other"),
                        };
                        (selector, None)
                    }
                };
                let body = options
                    .iter()
                    .find(|(option, _)| option == &selector)
                    .or_else(|| options.iter().find(|(option, _)| option == "other"))
                    .map(|(_, body)| body);
                if let Some(body) = body {
                    render_segments(body, args, policy, out, shadow.or(number_shadow));
                }
            }
        }
    }
}

/// Cardinal plural categories for the supported languages. English has
/// `one`/`other`; Japanese has no grammatical number (`other` only) — the
/// same category function drives counters via `#`.
fn plural_selector(n: f64) -> String {
    if (n - 1.0).abs() < f64::EPSILON {
        "one".to_string()
    } else {
        "other".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::formatting::FormattingPolicy;
    use crate::domain::locale::Locale;

    fn policy(locale: &str) -> FormattingPolicy {
        FormattingPolicy::for_locale(Locale::parse(locale).unwrap(), "UTC").unwrap()
    }

    fn args(items: Vec<(&str, Arg)>) -> HashMap<String, Arg> {
        items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    fn render_pattern(pattern: &str, locale: &str, items: Vec<(&str, Arg)>) -> String {
        let segments = parse_pattern(pattern).unwrap();
        render(&segments, &args(items), &policy(locale))
    }

    #[test]
    fn plain_text_and_substitution() {
        assert_eq!(
            render_pattern("Hello, {name}!", "en", vec![("name", "Alice".into())]),
            "Hello, Alice!"
        );
    }

    #[test]
    fn english_plurals_cover_zero_one_many() {
        let pattern = "{count, plural, one {# item} other {# items}}";
        assert_eq!(
            render_pattern(pattern, "en", vec![("count", 0i64.into())]),
            "0 items"
        );
        assert_eq!(
            render_pattern(pattern, "en", vec![("count", 1i64.into())]),
            "1 item"
        );
        assert_eq!(
            render_pattern(pattern, "en", vec![("count", 42i64.into())]),
            "42 items"
        );
    }

    #[test]
    fn japanese_counters_never_pluralize() {
        let pattern = "{count, plural, other {#日}}";
        for count in [0i64, 1, 7, 100] {
            let rendered = render_pattern(pattern, "ja", vec![("count", count.into())]);
            assert_eq!(rendered, format!("{count}日"));
        }
    }

    #[test]
    fn exact_matches_beat_categories() {
        let pattern = "{count, plural, =0 {none} one {one} other {#}}";
        assert_eq!(
            render_pattern(pattern, "en", vec![("count", 0i64.into())]),
            "none"
        );
    }

    #[test]
    fn offset_shifts_the_category_but_not_the_shadow() {
        // offset:1 removes one item from the category math: 2 → "one" but
        // the shadow (#) still renders the raw 2.
        let pattern = "{count, plural, offset:1 one {# item} other {# items}}";
        assert_eq!(
            render_pattern(pattern, "en", vec![("count", 2i64.into())]),
            "2 item"
        );
        assert_eq!(
            render_pattern(pattern, "en", vec![("count", 3i64.into())]),
            "3 items"
        );
    }

    #[test]
    fn select_picks_options_and_other() {
        let pattern = "{gender, select, female {She} male {He} other {They}}";
        assert_eq!(
            render_pattern(pattern, "en", vec![("gender", "female".into())]),
            "She"
        );
        assert_eq!(
            render_pattern(pattern, "en", vec![("gender", "x".into())]),
            "They"
        );
    }

    #[test]
    fn number_date_and_time_styles() {
        assert_eq!(
            render_pattern("{n, number}", "en", vec![("n", 1234567f64.into())]),
            "1,234,567"
        );
        let dt: Arg = DateTime::parse_from_rfc3339("2026-08-15T14:05:00Z")
            .unwrap()
            .with_timezone(&Utc)
            .into();
        assert_eq!(
            render_pattern("{d, date}", "ja", vec![("d", dt.clone())]),
            "2026年8月15日"
        );
        assert_eq!(
            render_pattern("{d, date}", "en", vec![("d", dt)]),
            "Aug 15, 2026"
        );
    }

    #[test]
    fn apostrophe_quoting_rules() {
        assert_eq!(render_pattern("Don''t worry", "en", vec![]), "Don't worry");
    }

    #[test]
    fn missing_arguments_are_visible_not_silent() {
        assert_eq!(render_pattern("Hi {name}", "en", vec![]), "Hi {name}");
    }

    #[test]
    fn rejects_broken_patterns() {
        assert!(parse_pattern("{name").is_err());
        assert!(parse_pattern("{name, plural}").is_err());
        assert!(parse_pattern("{name, wat}").is_err());
        assert!(parse_pattern("{n, plural, one {x}").is_err());
        assert!(parse_pattern("{n, plural, one {one}}").is_err());
        assert!(parse_pattern("{kind, select, known {known}}").is_err());
    }

    proptest::proptest! {
        #[test]
        fn parsing_never_panics(pattern in "\\PC{0,80}") {
            let _ = parse_pattern(&pattern);
        }
    }
}
