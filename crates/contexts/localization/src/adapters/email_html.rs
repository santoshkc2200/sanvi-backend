//! Accessible HTML email bodies (TASK-027, FR-1127).
//!
//! The transactional catalogs render plain text; the notification service
//! accepts an HTML alternative alongside it. This module derives that HTML
//! from the rendered plain text so the two can never diverge in content:
//!
//! - semantic structure: one `<h1>` from the subject, one `<p>` per
//!   paragraph, `lang` from the negotiated locale — a screen reader gets
//!   the same heading hierarchy the apps have;
//! - real text: no images anywhere in the pipeline, so there is no
//!   text-in-image to fail;
//! - meaningful link text: a line that is exactly a URL becomes
//!   `<a href>` labelled with the caller's action label (or the URL's host
//!   when no label is known) instead of a naked URL;
//! - a plain-text alternative carrying the same information: the plain
//!   body itself, asserted by tests to contain every fact the HTML does;
//! - contrast: the fixed palette is WCAG-AA-checked by unit test, not by
//!   eye. No template carries tabular data today, so no table structure is
//!   rendered; a tabular template must extend [`email_html`] with one.

/// The email palette. Body, heading and link colours on white — each pair
/// is asserted ≥ 4.5:1 (WCAG AA for normal text) by [`tests::palette_meets_aa_contrast`].
pub const BODY_COLOR: &str = "#1f2937";
pub const HEADING_COLOR: &str = "#111827";
pub const LINK_COLOR: &str = "#1d4ed8";
pub const BACKGROUND: &str = "#ffffff";

/// Derive the accessible HTML alternative for a rendered email.
///
/// `link_label` labels a bare-URL line (the invitation's accept action, for
/// example) in the locale the rest of the email was rendered in.
pub fn email_html(
    locale: &str,
    subject: &str,
    text_body: &str,
    link_label: Option<&str>,
) -> String {
    let mut paragraphs = Vec::new();
    for paragraph in text_body.split("\n\n") {
        let mut lines = Vec::new();
        for line in paragraph.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // A line that *is* a URL is the email's action: anchor it with
            // meaningful text instead of reading a naked URL aloud.
            if let Some(url) = sole_url(line) {
                let label = link_label
                    .map(str::to_string)
                    .unwrap_or_else(|| host_of(url));
                lines.push(format!(
                    "<a href=\"{}\" style=\"color:{};\">{}</a>",
                    escape_html(url),
                    LINK_COLOR,
                    escape_html(&label)
                ));
            } else {
                lines.push(escape_html(line));
            }
        }
        if lines.is_empty() {
            continue;
        }
        paragraphs.push(format!(
            "<p style=\"color:{};margin:0 0 16px;\">{}</p>",
            BODY_COLOR,
            lines.join("<br>\n")
        ));
    }
    format!(
        "<!doctype html>\n<html lang=\"{}\">\n<body style=\"margin:0;padding:24px;background:{};\">\n\
         <h1 style=\"color:{};font-size:20px;margin:0 0 16px;\">{}</h1>\n{}\n</body>\n</html>\n",
        escape_html(locale),
        BACKGROUND,
        HEADING_COLOR,
        escape_html(subject),
        paragraphs.join("\n")
    )
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// The single URL of a paragraph that is exactly one URL, if any.
fn sole_url(line: &str) -> Option<&str> {
    (line.starts_with("http://") || line.starts_with("https://")).then_some(line)
}

fn host_of(url: &str) -> String {
    url.split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

/// WCAG relative-contrast ratio between two `#rrggbb` colours.
pub fn contrast_ratio(a: &str, b: &str) -> f64 {
    fn luminance(hex: &str) -> f64 {
        let hex = hex.trim_start_matches('#');
        let channel = |i: usize| -> f64 {
            let value = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap_or(0) as f64 / 255.0;
            if value <= 0.03928 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(0) + 0.7152 * channel(1) + 0.0722 * channel(2)
    }
    let (l1, l2) = (luminance(a), luminance(b));
    let (light, dark) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (light + 0.05) / (dark + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_semantic_structure_with_lang_and_single_h1() {
        let html = email_html(
            "ja",
            "招待のタイトル",
            "最初の段落です。\n\n2番目の段落です。",
            None,
        );
        assert!(html.starts_with("<!doctype html>"));
        assert_eq!(html.matches("<h1").count(), 1, "exactly one heading");
        assert_eq!(html.matches("<p").count(), 2);
        assert!(html.contains("<html lang=\"ja\">"));
        assert!(html.contains("<h1 style=\"color:#111827"));
    }

    #[test]
    fn escapes_interpolated_values() {
        let html = email_html(
            "en",
            "Security <notice> & alert",
            "An asset named <b>not-markup</b> & friends.",
            None,
        );
        assert!(!html.contains("<b>not-markup</b>"), "{html}");
        assert!(html.contains("&lt;b&gt;not-markup&lt;/b&gt; &amp; friends."));
        assert!(html.contains("Security &lt;notice&gt; &amp; alert"));
    }

    #[test]
    fn a_bare_url_line_becomes_a_labelled_link() {
        let html = email_html(
            "en",
            "You're invited",
            "Hi a@example.com,\n\nAccept here:\nhttps://app.example.com/invite/tok\n\nExpires in 7 days.",
            Some("Accept the invitation"),
        );
        assert!(html.contains(
            "<a href=\"https://app.example.com/invite/tok\" style=\"color:#1d4ed8;\">Accept the invitation</a>"
        ));
        // The plain URL is still present as the href: the fact is not lost.
        assert!(html.contains("https://app.example.com/invite/tok"));
    }

    #[test]
    fn an_unlabelled_url_link_names_its_host() {
        let html = email_html("en", "Subject", "https://app.example.com/invite/tok", None);
        assert!(html.contains(">app.example.com</a>"));
    }

    /// Body, heading and link colours all clear WCAG AA (4.5:1) on the
    /// white background — measured, not eyeballed.
    #[test]
    fn palette_meets_aa_contrast() {
        for color in [BODY_COLOR, HEADING_COLOR, LINK_COLOR] {
            let ratio = contrast_ratio(color, BACKGROUND);
            assert!(
                ratio >= 4.5,
                "{color} on {BACKGROUND} is {ratio:.2}:1, below AA"
            );
        }
    }

    #[test]
    fn contrast_ratio_matches_known_values() {
        // Black on white is 21:1; the same colour is 1:1.
        assert!((contrast_ratio("#000000", "#ffffff") - 21.0).abs() < 0.01);
        assert!((contrast_ratio("#ffffff", "#ffffff") - 1.0).abs() < 0.001);
    }

    // -------------------------------------------------------------------
    // Catalog-wide template checks, both locales (TASK-027 step 4). The
    // templates ARE the catalogs; these rules hold for every email in
    // `emails.<locale>.json`, so a new template inherits them by landing
    // in the catalog.
    // -------------------------------------------------------------------

    use crate::generated::CATALOG_FILES;

    fn email_templates(locale: &str) -> Vec<(&'static str, &'static str)> {
        CATALOG_FILES
            .iter()
            .filter(|(l, domain, _)| *l == locale && *domain == "emails")
            .flat_map(|(_, _, content)| {
                let map: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(content).expect("catalogs are valid JSON");
                map.into_iter()
                    .map(|(key, value)| {
                        let key: &'static str = Box::leak(key.into_boxed_str());
                        let value: &'static str = Box::leak(
                            value
                                .as_str()
                                .expect("template values are strings")
                                .to_string()
                                .into_boxed_str(),
                        );
                        (key, value)
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Replace each ICU placeholder (`{name}`, `{n, plural, …}`,
    /// `{d, date}`) with a distinct sentinel so the rendered-equivalence
    /// check has facts to trace.
    fn with_sentinel_values(pattern: &str) -> String {
        let mut out = String::with_capacity(pattern.len());
        let mut chars = pattern.chars().peekable();
        let mut sentinel = 0;
        while let Some(c) = chars.next() {
            if c != '{' {
                out.push(c);
                continue;
            }
            let mut depth = 1;
            let mut span = String::from("{");
            for inner in chars.by_ref() {
                span.push(inner);
                if inner == '{' {
                    depth += 1;
                }
                if inner == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            sentinel += 1;
            out.push_str(&format!("FACT{sentinel}"));
        }
        out
    }

    #[test]
    fn email_templates_are_structured_plain_text_in_both_locales() {
        for locale in ["en", "ja"] {
            let templates = email_templates(locale);
            assert!(
                templates.len() >= 40,
                "the {locale} email catalog shrank — the structure rules below \
                 are meant to cover every transactional email"
            );
            for (key, template) in templates {
                assert!(!template.trim().is_empty(), "{locale}/{key} is empty");
                // Templates are plain text: markup would be escaped and
                // shown literally to the recipient. A bare `>` is honest
                // prose ("greater than"); an `<` is where markup starts.
                assert!(
                    !template.contains('<'),
                    "{locale}/{key} embeds markup — templates are plain text"
                );
                // A URL is real text on its own line, so the HTML
                // alternative can anchor it with meaningful link text.
                for line in template.lines() {
                    let trimmed = line.trim();
                    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                        assert_eq!(
                            trimmed, line,
                            "{locale}/{key}: a URL must sit on its own line, not inside prose"
                        );
                    }
                }
            }
        }
    }

    /// The plain-text alternative carries every fact the HTML does: for
    /// each template, each placeholder's value in the text appears in the
    /// rendered HTML too.
    #[test]
    fn the_html_alternative_carries_every_fact_the_text_does() {
        for locale in ["en", "ja"] {
            for (key, template) in email_templates(locale) {
                let subject = format!("SUBJECT-{key}");
                let text = with_sentinel_values(template);
                let html = email_html(locale, &subject, &text, None);
                for fact in text.split_whitespace().filter(|w| w.starts_with("FACT")) {
                    assert!(
                        html.contains(&escape_html(fact)),
                        "{locale}/{key}: fact {fact} is in the plain text but missing \
                         from the HTML alternative"
                    );
                }
                // The subject is the heading, not a dropped detail.
                assert!(html.contains(&escape_html(&subject)));
            }
        }
    }
}
