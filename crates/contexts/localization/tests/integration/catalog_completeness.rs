//! Catalog completeness: every shipped locale covers every key of every
//! shipped domain — the same gate `cargo i18n check` enforces in CI, also
//! proven inside the crate so a missing translation fails `cargo test`.

use std::collections::{BTreeMap, BTreeSet};

use sanvi_localization::generated::{CATALOG_FILES, KNOWN_KEYS, SHIPPED_LOCALES};

/// (locale, domain) → key set.
fn parsed() -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (locale, domain, content) in CATALOG_FILES {
        let map: BTreeMap<String, serde_json::Value> =
            serde_json::from_str(content).expect("embedded catalog is valid JSON");
        out.insert(
            (locale.to_string(), domain.to_string()),
            map.keys().cloned().collect(),
        );
    }
    out
}

#[test]
fn every_shipped_locale_covers_the_source_key_set_per_domain() {
    let catalogs = parsed();
    for domain in ["billing", "emails", "errors", "privacy"] {
        let source = catalogs
            .get(&("en".to_string(), domain.to_string()))
            .expect("source catalog missing");

        for locale in SHIPPED_LOCALES {
            if *locale == "en" {
                continue;
            }
            let key = (locale.to_string(), domain.to_string());
            let present = catalogs
                .get(&key)
                .unwrap_or_else(|| panic!("catalog file missing: {locale}/{domain}.json"));
            let missing: Vec<&String> = source.difference(present).collect();
            assert!(
                missing.is_empty(),
                "{locale}/{domain} is missing {} key(s): {missing:?}",
                missing.len()
            );
            let extra: Vec<&String> = present.difference(source).collect();
            assert!(
                extra.is_empty(),
                "{locale}/{domain} has extra key(s) the source lacks: {extra:?}"
            );
        }
    }
}

#[test]
fn every_key_has_a_non_empty_value_and_valid_icu() {
    for (locale, domain, content) in CATALOG_FILES {
        let map: BTreeMap<String, String> = serde_json::from_str(content).unwrap();
        for (key, pattern) in &map {
            assert!(!pattern.is_empty(), "{locale}/{domain}/{key} is empty");
            sanvi_localization::parse_pattern(pattern)
                .unwrap_or_else(|err| panic!("{locale}/{domain}/{key} does not parse: {err}"));
        }
    }
}

#[test]
fn known_keys_cover_all_embedded_keys() {
    let mut all: BTreeSet<String> = BTreeSet::new();
    for (_, _, content) in CATALOG_FILES {
        let map: BTreeMap<String, serde_json::Value> = serde_json::from_str(content).unwrap();
        all.extend(map.keys().cloned());
    }
    let known: BTreeSet<&str> = KNOWN_KEYS.iter().copied().collect();
    let all_refs: BTreeSet<&str> = all.iter().map(String::as_str).collect();
    assert_eq!(all_refs, known, "the generated KNOWN_KEYS table drifted");
}

#[test]
fn every_translation_has_exact_placeholder_parity_with_english() {
    let mut catalogs: BTreeMap<(String, String), BTreeMap<String, String>> = BTreeMap::new();
    for (locale, domain, content) in CATALOG_FILES {
        catalogs.insert(
            (locale.to_string(), domain.to_string()),
            serde_json::from_str(content).unwrap(),
        );
    }
    for domain in ["billing", "emails", "errors", "privacy"] {
        let source = &catalogs[&("en".to_string(), domain.to_string())];
        for locale in SHIPPED_LOCALES
            .iter()
            .copied()
            .filter(|locale| *locale != "en")
        {
            let target = &catalogs[&(locale.to_string(), domain.to_string())];
            for (key, source_pattern) in source {
                let source_segments = sanvi_localization::parse_pattern(source_pattern).unwrap();
                let target_segments = sanvi_localization::parse_pattern(&target[key]).unwrap();
                let source_names =
                    sanvi_localization::domain::message_format::placeholder_names(&source_segments);
                let target_names =
                    sanvi_localization::domain::message_format::placeholder_names(&target_segments);
                assert_eq!(
                    source_names, target_names,
                    "{locale}/{domain}/{key} placeholder mismatch"
                );
            }
        }
    }
}
