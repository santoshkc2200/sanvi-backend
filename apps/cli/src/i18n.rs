//! `i18n`: the translation workflow gate.
//!
//! - `extract` — scan the source tree for `t!("key")` invocations and print
//!   the key → file map as JSON.
//! - `check` — the CI hard gate: catalogs parse, `en`/`ja` are in lock-step
//!   on every shipped domain, every source key exists in the `en` catalog,
//!   and every translation references the same placeholders as its source.
//! - `report` — per-domain coverage table.
//!
//! How a string gets from code to a translated catalog is documented in
//! `docs/phase-06-i18n-l10n/translation-workflow.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// The catalogs directory inside the localization crate.
const CATALOGS_DIR: &str = "crates/contexts/localization/catalogs";

/// The domains that gate at 100 % coverage (everything the binary ships).
const SHIPPED_DOMAINS: &[&str] = &["billing", "emails", "errors", "payments", "privacy"];

/// The workspace root (this binary is `apps/cli`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn catalogs_dir() -> PathBuf {
    workspace_root().join(CATALOGS_DIR)
}

/// (locale, domain) → (key → pattern).
fn load_catalogs() -> anyhow::Result<BTreeMap<(String, String), BTreeMap<String, String>>> {
    let root = catalogs_dir();
    let mut out = BTreeMap::new();
    for locale_entry in std::fs::read_dir(&root)
        .with_context(|| format!("cannot read catalogs directory {root:?}"))?
    {
        let locale_dir = locale_entry?.path();
        if !locale_dir.is_dir() {
            continue;
        }
        let locale = locale_dir
            .file_name()
            .expect("locale dir name")
            .to_str()
            .expect("locale name is utf-8")
            .to_string();
        for file in std::fs::read_dir(&locale_dir)? {
            let path = file?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let domain = path
                .file_stem()
                .expect("catalog file stem")
                .to_str()
                .expect("catalog name is utf-8")
                .to_string();
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read catalog {path:?}"))?;
            let map: BTreeMap<String, String> = serde_json::from_str(&content)
                .with_context(|| format!("catalog {path:?} is not a flat string map"))?;
            out.insert((locale.clone(), domain), map);
        }
    }
    Ok(out)
}

/// Every `t!("literal")` key in the workspace source, keyed by file.
fn extract_keys() -> anyhow::Result<BTreeMap<String, BTreeSet<PathBuf>>> {
    let root = workspace_root();
    let mut out: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    for dir in ["crates", "apps"] {
        let base = root.join(dir);
        if !base.exists() {
            continue;
        }
        walk_rs_files(&base, &mut |path| {
            let content = std::fs::read_to_string(path)?;
            for key in t_keys_in(&content) {
                out.entry(key).or_default().insert(path.to_path_buf());
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn walk_rs_files(
    dir: &Path,
    visit: &mut dyn FnMut(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            walk_rs_files(&path, visit)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            visit(&path)?;
        }
    }
    Ok(())
}

/// Extract string-literal keys from `t!(...)` invocations in one file.
/// Grep-grade on purpose: the `t!` macro's compile-time check is the real
/// gate; this extraction drives the coverage report. Line comments are
/// stripped first (doc examples must not count as usage).
fn t_keys_in(content: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for line in content.lines() {
        let code = match line.find("//") {
            Some(index) => &line[..index],
            None => line,
        };
        let mut rest = code;
        while let Some(index) = rest.find("t!(") {
            // `format!(` must not match: the character before `t!` must not
            // be part of an identifier.
            let is_real_macro = rest[..index]
                .chars()
                .next_back()
                .map(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .unwrap_or(true);
            let after = &rest[index + 3..];
            let mut end = None;
            for (i, ch) in after.char_indices() {
                if ch == ')' {
                    end = Some(i);
                    break;
                }
                if ch == ';' || ch == '\n' {
                    break;
                }
            }
            if is_real_macro {
                let invocation = end.map(|i| &after[..i]).unwrap_or(after);
                for candidate in invocation.split(',') {
                    let candidate = candidate.trim();
                    let Some(key) = candidate
                        .strip_prefix('"')
                        .and_then(|s| s.strip_suffix('"'))
                    else {
                        continue;
                    };
                    if key.contains(' ') || key.contains('{') {
                        continue;
                    }
                    keys.insert(key.to_string());
                }
            }
            rest = &after[end.unwrap_or(after.len())..];
        }
    }
    keys
}

/// Placeholder names a pattern references (shared with the crate's parser).
fn placeholders(pattern: &str) -> anyhow::Result<BTreeSet<String>> {
    let segments =
        sanvi_localization::parse_pattern(pattern).map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(sanvi_localization::domain::message_format::placeholder_names(&segments))
}

fn placeholder_differences(
    source: &BTreeSet<String>,
    target: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    (
        source.difference(target).cloned().collect(),
        target.difference(source).cloned().collect(),
    )
}

pub fn run(action: I18nAction) -> anyhow::Result<()> {
    match action {
        I18nAction::Extract => {
            let keys = extract_keys()?;
            let mut report: BTreeMap<&str, Vec<&PathBuf>> = BTreeMap::new();
            for (key, files) in &keys {
                report.insert(key, files.iter().collect());
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        I18nAction::Check => check(),
        I18nAction::Report => report(),
    }
}

fn check() -> anyhow::Result<()> {
    let catalogs = load_catalogs()?;
    let mut problems: Vec<String> = Vec::new();

    // 1. Every catalog parses; every value is non-empty.
    for ((locale, domain), map) in &catalogs {
        for (key, pattern) in map {
            if pattern.is_empty() {
                problems.push(format!("{locale}/{domain}/{key}: empty value"));
                continue;
            }
            if let Err(err) = sanvi_localization::parse_pattern(pattern) {
                problems.push(format!("{locale}/{domain}/{key}: {err}"));
            }
        }
    }

    // 2. Shipped domains: en and ja in lock-step at 100 %.
    for domain in SHIPPED_DOMAINS {
        let source = match catalogs.get(&("en".to_string(), domain.to_string())) {
            Some(map) => map,
            None => {
                problems.push(format!("en/{domain}.json is missing"));
                continue;
            }
        };
        let ja = match catalogs.get(&("ja".to_string(), domain.to_string())) {
            Some(map) => map,
            None => {
                problems.push(format!("ja/{domain}.json is missing"));
                continue;
            }
        };
        let missing: Vec<&String> = source.keys().filter(|key| !ja.contains_key(*key)).collect();
        for key in &missing {
            problems.push(format!("ja/{domain} is missing key {key}"));
        }
        let extra: Vec<&String> = ja.keys().filter(|key| !source.contains_key(*key)).collect();
        for key in &extra {
            problems.push(format!("ja/{domain} has an extra key {key}"));
        }

        // 3. Translations reference the same placeholders as the source.
        for (key, source_pattern) in source {
            let Some(target) = ja.get(key) else {
                continue;
            };
            let source_placeholders = placeholders(source_pattern)?;
            let target_placeholders = placeholders(target)?;
            let (missing, unexpected) =
                placeholder_differences(&source_placeholders, &target_placeholders);
            for placeholder in &missing {
                problems.push(format!(
                    "ja/{domain}/{key} is missing placeholder {{{placeholder}}}"
                ));
            }
            for placeholder in &unexpected {
                problems.push(format!(
                    "ja/{domain}/{key} has unexpected placeholder {{{placeholder}}}"
                ));
            }
        }
    }

    // 4. Every source key used in code exists in the en catalog.
    let mut en_keys: BTreeSet<&str> = BTreeSet::new();
    for domain in SHIPPED_DOMAINS {
        if let Some(map) = catalogs.get(&("en".to_string(), domain.to_string())) {
            en_keys.extend(map.keys().map(String::as_str));
        }
    }
    let source_keys = extract_keys()?;
    for key in source_keys.keys() {
        if !en_keys.contains(key.as_str()) {
            problems.push(format!(
                "key {key:?} is used in code but missing from the en catalog"
            ));
        }
    }

    if !problems.is_empty() {
        bail!(
            "i18n check failed:\n  - {}\n\nRun `cargo run -p sanvi-cli -- i18n report` for coverage.",
            problems.join("\n  - ")
        );
    }
    println!(
        "i18n check OK: {} catalogs, {} source keys, 100 % ja coverage on shipped domains",
        catalogs.len(),
        source_keys.len()
    );
    Ok(())
}

fn report() -> anyhow::Result<()> {
    let catalogs = load_catalogs()?;
    let mut by_domain: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for ((locale, domain), map) in &catalogs {
        let entry = by_domain.entry(domain.clone()).or_insert((0, 0));
        if locale == "en" {
            entry.0 = map.len();
        } else if locale == "ja" {
            entry.1 = map.len();
        }
    }
    println!(
        "{:<12} {:>8} {:>8} {:>10}",
        "domain", "en", "ja", "coverage"
    );
    for (domain, (en, ja)) in &by_domain {
        let coverage = if *en == 0 {
            0.0
        } else {
            *ja as f64 / *en as f64 * 100.0
        };
        let marker = if SHIPPED_DOMAINS.contains(&domain.as_str()) && *ja < *en {
            "  <-- gated"
        } else {
            ""
        };
        println!("{domain:<12} {en:>8} {ja:>8} {coverage:>9.1}%{marker}");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I18nAction {
    Extract,
    Check,
    Report,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_literal_keys_from_source() {
        let content = r#"
            let msg = t!("emails.invitation.subject", tenant = "Acme");
            let bare = t!("errors.title.error");
            let other = t!( "billing.invoice.label" );
        "#;
        let keys = t_keys_in(content);
        assert!(keys.contains("emails.invitation.subject"));
        assert!(keys.contains("errors.title.error"));
        assert!(keys.contains("billing.invoice.label"));
    }

    #[test]
    fn ignores_dynamic_keys() {
        let content = r#"translator.translate(scope, locale, key, &[]);"#;
        assert!(t_keys_in(content).is_empty());
    }

    #[test]
    fn placeholder_parity_rejects_missing_and_unexpected_names() {
        let source = placeholders("Hi {name}, {count, plural, other {# items}}").unwrap();
        let target = placeholders("こんにちは {person}、{count, plural, other {#件}}").unwrap();
        let (missing, unexpected) = placeholder_differences(&source, &target);
        assert_eq!(missing, vec!["name"]);
        assert_eq!(unexpected, vec!["person"]);
    }
}
