//! Locale identifiers and negotiation.
//!
//! [`Locale`] is a validated, canonicalised BCP-47 tag (`en`, `en-GB`,
//! `ja-JP`). [`negotiate`] implements the deterministic precedence from the
//! phase-06 plan:
//!
//! 1. explicit `?locale=` (allow-listed against the platform set)
//! 2. the authenticated user's preference
//! 3. the tenant's configured default
//! 4. the best `Accept-Language` match
//! 5. the platform default
//!
//! Every accepted value is normalised and validated; a value that fails
//! validation or is not in the applicable set is ignored (the chain
//! continues), so negotiation always terminates at a known-good locale.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::LocalizationError;

/// How negotiation settled — the highest-precedence source that produced a
/// usable value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocaleSource {
    Explicit,
    UserPreference,
    TenantDefault,
    AcceptLanguage,
    PlatformDefault,
}

impl fmt::Display for LocaleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Explicit => write!(f, "explicit"),
            Self::UserPreference => write!(f, "user_preference"),
            Self::TenantDefault => write!(f, "tenant_default"),
            Self::AcceptLanguage => write!(f, "accept_language"),
            Self::PlatformDefault => write!(f, "platform_default"),
        }
    }
}

/// A validated, canonicalised BCP-47 language tag.
///
/// Canonicalisation: language lowercase, script title-case, region
/// uppercase (`ja-jp` → `ja-JP`, `EN` → `en`). Only well-formed tags
/// accepted: 2–3 letter language, optional 4-letter script, optional 2-letter
/// or 3-digit region. This is deliberately *not* full BCP-47 (no variants,
/// no extensions): the backend only needs the granularity it can translate
/// and format with.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[schema(as = LocalizationLocale)]
pub struct Locale(String);

impl Locale {
    pub const DEFAULT: &'static str = "en";

    /// Parse, validate and canonicalise.
    pub fn parse(input: &str) -> Result<Self, LocalizationError> {
        if input.len() < 2 || input.len() > 35 {
            return Err(LocalizationError::InvalidLocale(input.to_string()));
        }
        let mut parts = input.split('-');
        let language = parts.next().unwrap_or_default();
        if !(2..=3).contains(&language.len()) || !language.chars().all(|c| c.is_ascii_alphabetic())
        {
            return Err(LocalizationError::InvalidLocale(input.to_string()));
        }
        let mut canonical = language.to_ascii_lowercase();

        let mut rest: Vec<&str> = parts.collect();
        if !rest.is_empty() {
            let last = rest.pop().unwrap_or_default();
            let is_region = (last.len() == 2 && last.chars().all(|c| c.is_ascii_alphabetic()))
                || (last.len() == 3 && last.chars().all(|c| c.is_ascii_digit()));
            let is_script = last.len() == 4 && last.chars().all(|c| c.is_ascii_alphabetic());
            if !is_region && !is_script {
                return Err(LocalizationError::InvalidLocale(input.to_string()));
            }
            // Any remaining parts (between language and the last subtag)
            // must be scripts: 4 letters, title-cased (`zh-Hant-CN`).
            let mut ordered: Vec<String> = Vec::new();
            for part in rest {
                if part.len() != 4 || !part.chars().all(|c| c.is_ascii_alphabetic()) {
                    return Err(LocalizationError::InvalidLocale(input.to_string()));
                }
                ordered.push(title_case(part));
            }
            // The final subtag: region upper, script title-case.
            ordered.push(if is_region {
                last.to_ascii_uppercase()
            } else {
                title_case(last)
            });
            for part in ordered {
                canonical.push('-');
                canonical.push_str(&part);
            }
        }
        Ok(Self(canonical))
    }

    /// The primary language subtag: `en` from `en-GB`, `ja` from `ja-JP`.
    pub fn language(&self) -> &str {
        self.0.split('-').next().unwrap_or(&self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Locale {
    fn default() -> Self {
        Self::parse(Self::DEFAULT).expect("default locale is valid")
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for Locale {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Script casing: first letter upper, rest lower (`HANT` → `Hant`).
fn title_case(input: &str) -> String {
    let mut chars = input.chars();
    match chars.next() {
        Some(first) => {
            let mut out = first.to_uppercase().collect::<String>();
            out.extend(chars.flat_map(|c| c.to_lowercase()));
            out
        }
        None => String::new(),
    }
}

/// A set of supported locales with a default — the shape both the platform
/// and individual tenants expose to negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleSet {
    pub supported: Vec<Locale>,
    pub default: Locale,
}

impl LocaleSet {
    pub fn new(supported: Vec<Locale>, default: Locale) -> Result<Self, LocalizationError> {
        if !supported.contains(&default) {
            return Err(LocalizationError::SettingsConflict(format!(
                "default locale {default} is not in the supported set"
            )));
        }
        Ok(Self { supported, default })
    }

    pub fn single(locale: Locale) -> Self {
        Self {
            supported: vec![locale.clone()],
            default: locale,
        }
    }

    pub fn contains(&self, locale: &Locale) -> bool {
        self.supported.contains(locale)
    }

    pub fn find(&self, tag: &str) -> Option<Locale> {
        self.supported
            .iter()
            .find(|locale| locale.as_str() == tag)
            .cloned()
    }
}

/// Everything negotiation is allowed to look at. Plain data: the caller
/// owns every backend lookup (user preference, tenant settings).
#[derive(Debug, Clone)]
pub struct NegotiationInput<'a> {
    /// `?locale=` query parameter (untrusted user input).
    pub explicit: Option<&'a str>,
    /// The authenticated user's stored preference.
    pub user_preference: Option<&'a str>,
    /// The tenant's configured default.
    pub tenant_default: Option<&'a str>,
    /// The raw `Accept-Language` header.
    pub accept_language: Option<&'a str>,
    /// The platform default.
    pub platform_default: &'a str,
    /// What the tenant allows (falls back to the platform set).
    pub supported: LocaleSet,
    /// The configured explicit-query allow-list, intersected with shipped
    /// platform locales by the resolver.
    pub platform_supported: LocaleSet,
}

/// The outcome: the negotiated locale, its source and the fallback chain
/// per-key lookup walks (most specific first, platform default last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedLocale {
    pub locale: Locale,
    pub source: LocaleSource,
    pub fallback_chain: Vec<Locale>,
}

/// The deterministic negotiation algorithm. Never fails: the platform
/// default is always a member of the platform set (a config invariant), so
/// step 5 always yields a usable locale.
pub fn negotiate(input: &NegotiationInput<'_>) -> NegotiatedLocale {
    let platform_default = Locale::parse(input.platform_default).unwrap_or_default();

    let accept = |raw: Option<&str>, source| -> Option<(Locale, LocaleSource)> {
        let locale = Locale::parse(raw?);
        let locale = match locale {
            Ok(locale) => locale,
            Err(_) => return None,
        };
        Some((locale, source))
    };

    // 1. Explicit query parameter: allow-listed against the platform set so
    //    a hostile `?locale=../../etc` or a bogus tag can never influence
    //    rendering beyond what the platform ships.
    if let Some((locale, source)) = accept(input.explicit, LocaleSource::Explicit)
        && input.platform_supported.contains(&locale)
    {
        return finish(locale, source, input);
    }

    // 2. The signed-in user's preference (tenant-gated).
    if let Some((locale, source)) = accept(input.user_preference, LocaleSource::UserPreference)
        && input.supported.contains(&locale)
    {
        return finish(locale, source, input);
    }

    // 3. The tenant default.
    if let Some((locale, source)) = accept(input.tenant_default, LocaleSource::TenantDefault)
        && input.supported.contains(&locale)
    {
        return finish(locale, source, input);
    }

    // 4. Best Accept-Language match: quality-sorted, exact locale match
    //    first, then same-language (preferring regional specificity).
    if let Some(raw) = input.accept_language
        && let Some(locale) = best_accept_language(raw, &input.supported)
    {
        return finish(locale, LocaleSource::AcceptLanguage, input);
    }

    // 5. Platform default (guaranteed present).
    finish(platform_default, LocaleSource::PlatformDefault, input)
}

fn finish(locale: Locale, source: LocaleSource, input: &NegotiationInput<'_>) -> NegotiatedLocale {
    let mut chain = vec![locale.clone()];
    let language = locale.language().to_string();
    if language != locale.as_str()
        && let Ok(base) = Locale::parse(&language)
    {
        chain.push(base);
    }
    if let Ok(default) = Locale::parse(input.platform_default)
        && !chain.contains(&default)
    {
        chain.push(default);
    }
    NegotiatedLocale {
        locale,
        source,
        fallback_chain: chain,
    }
}

/// Parse an `Accept-Language` header into quality-sorted candidates and
/// pick the best match against the supported set: the highest-quality
/// candidate that matches (exact locale, else its language) wins.
fn best_accept_language(raw: &str, supported: &LocaleSet) -> Option<Locale> {
    let mut candidates: Vec<(f64, usize, String)> = Vec::new();
    for (index, entry) in raw.split(',').enumerate() {
        let mut parts = entry.split(';');
        let tag = parts.next().unwrap_or_default().trim();
        if tag.is_empty() || tag == "*" {
            continue;
        }
        let quality = match parts.find_map(|param| {
            let (key, value) = param.trim().split_once('=')?;
            (key.trim() == "q").then(|| value.trim().parse::<f64>())
        }) {
            Some(Ok(quality)) if quality.is_finite() && quality > 0.0 && quality <= 1.0 => quality,
            Some(_) => continue,
            None => 1.0,
        };
        candidates.push((quality, index, tag.to_string()));
    }
    // Highest quality first; the header order breaks ties.
    candidates.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });

    // First candidate that matches wins: an exact locale match, else a
    // language match (the shipped locale for that language).
    for (_, _, tag) in &candidates {
        if let Ok(locale) = Locale::parse(tag)
            && supported.contains(&locale)
        {
            return Some(locale);
        }
        let language = tag.split('-').next().unwrap_or_default();
        if let Some(locale) = supported.find(language) {
            return Some(locale);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_set() -> LocaleSet {
        LocaleSet::new(
            vec![
                Locale::parse("en").unwrap(),
                Locale::parse("ja").unwrap(),
                Locale::parse("en-GB").unwrap(),
            ],
            Locale::parse("en").unwrap(),
        )
        .unwrap()
    }

    fn input<'a>(explicit: Option<&'a str>) -> NegotiationInput<'a> {
        NegotiationInput {
            explicit,
            user_preference: None,
            tenant_default: None,
            accept_language: None,
            platform_default: "en",
            supported: platform_set(),
            platform_supported: platform_set(),
        }
    }

    #[test]
    fn parses_and_canonicalises() {
        assert_eq!(Locale::parse("ja-jp").unwrap().as_str(), "ja-JP");
        assert_eq!(Locale::parse("EN").unwrap().as_str(), "en");
        assert_eq!(Locale::parse("en-gb").unwrap().as_str(), "en-GB");
        assert_eq!(Locale::parse("zh-Hant").unwrap().as_str(), "zh-Hant");
        assert_eq!(Locale::parse("ja").unwrap().language(), "ja");
        assert_eq!(Locale::parse("en-GB").unwrap().language(), "en");
    }

    #[test]
    fn rejects_malformed_tags() {
        for bad in [
            "",
            "e",
            "toolongtoolongtoolongtoolongtoolongtoolongtoolong",
            "-en",
            "en-",
            "en_US",
            "e n",
            "123",
            "en-xyzq-EFG",
            "en--GB",
        ] {
            assert!(
                Locale::parse(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn negotiation_matrix_all_five_sources_conflicting() {
        // Every source suggests a different locale; precedence decides.
        let input = NegotiationInput {
            explicit: Some("ja"),
            user_preference: Some("en-GB"),
            tenant_default: Some("en"),
            accept_language: Some("en-GB, en;q=0.5"),
            platform_default: "en",
            supported: platform_set(),
            platform_supported: platform_set(),
        };
        let result = negotiate(&input);
        assert_eq!(result.locale.as_str(), "ja");
        assert_eq!(result.source, LocaleSource::Explicit);
        assert_eq!(result.fallback_chain[0].as_str(), "ja");
        assert_eq!(result.fallback_chain[1].as_str(), "en");

        // Without the explicit parameter the user preference wins.
        let input = NegotiationInput {
            explicit: None,
            ..input
        };
        let result = negotiate(&input);
        assert_eq!(result.locale.as_str(), "en-GB");
        assert_eq!(result.source, LocaleSource::UserPreference);
        assert_eq!(result.fallback_chain[0].as_str(), "en-GB");
        assert_eq!(result.fallback_chain[1].as_str(), "en");

        // Then the tenant default…
        let input = NegotiationInput {
            user_preference: None,
            ..input
        };
        let result = negotiate(&input);
        assert_eq!(result.locale.as_str(), "en");
        assert_eq!(result.source, LocaleSource::TenantDefault);

        // …then Accept-Language…
        let input = NegotiationInput {
            tenant_default: None,
            ..input
        };
        let result = negotiate(&input);
        assert_eq!(result.locale.as_str(), "en-GB");
        assert_eq!(result.source, LocaleSource::AcceptLanguage);

        // …then the platform default.
        let input = NegotiationInput {
            accept_language: None,
            ..input
        };
        let result = negotiate(&input);
        assert_eq!(result.locale.as_str(), "en");
        assert_eq!(result.source, LocaleSource::PlatformDefault);
    }

    #[test]
    fn explicit_locale_is_allow_listed_against_the_platform_set() {
        let result = negotiate(&input(Some("fr-FR")));
        assert_eq!(result.locale.as_str(), "en");
        assert_eq!(result.source, LocaleSource::PlatformDefault);

        let mut restricted = input(Some("ja"));
        restricted.platform_supported = LocaleSet {
            supported: vec![Locale::parse("en").unwrap()],
            default: Locale::parse("en").unwrap(),
        };
        let result = negotiate(&restricted);
        assert_eq!(result.locale.as_str(), "en");
        assert_eq!(result.source, LocaleSource::PlatformDefault);

        let result = negotiate(&input(Some("../../evil")));
        assert_eq!(result.locale.as_str(), "en");
        assert_eq!(result.source, LocaleSource::PlatformDefault);
    }

    #[test]
    fn user_preference_outside_the_tenant_set_falls_through() {
        let mut input = input(None);
        input.user_preference = Some("fr");
        let result = negotiate(&input);
        assert_eq!(result.source, LocaleSource::PlatformDefault);
    }

    #[test]
    fn accept_language_matches_language_with_quality_order() {
        // `ja-JP` (q=0.8) beats `en` (q=0.4); the match renders as the
        // shipped `ja` locale.
        let result = negotiate(&NegotiationInput {
            accept_language: Some("fr;q=0.9, ja-JP;q=0.8, en;q=0.4"),
            ..input(None)
        });
        assert_eq!(result.locale.as_str(), "ja");
        assert_eq!(result.source, LocaleSource::AcceptLanguage);

        // A bare language tag maps onto the supported set entry.
        let result = negotiate(&NegotiationInput {
            accept_language: Some("ja, en;q=0.5"),
            ..input(None)
        });
        assert_eq!(result.locale.as_str(), "ja");

        // Unparseable header entries are skipped.
        let result = negotiate(&NegotiationInput {
            accept_language: Some("%%%%%, en"),
            ..input(None)
        });
        assert_eq!(result.locale.as_str(), "en");

        // q=0 means "not acceptable" and invalid quality values are
        // ignored rather than accidentally promoted above the fallback.
        let result = negotiate(&NegotiationInput {
            accept_language: Some("ja;q=0, en;q=0.5"),
            ..input(None)
        });
        assert_eq!(result.locale.as_str(), "en");
        let result = negotiate(&NegotiationInput {
            accept_language: Some("ja;q=2, en;q=0.5"),
            ..input(None)
        });
        assert_eq!(result.locale.as_str(), "en");
    }

    #[test]
    fn fallback_chain_never_repeats_and_ends_in_the_platform_default() {
        let result = negotiate(&input(Some("ja")));
        assert_eq!(
            result
                .fallback_chain
                .iter()
                .map(|locale| locale.as_str())
                .collect::<Vec<_>>(),
            vec!["ja", "en"]
        );
        // A regional variant: [en-GB, en] (en-GB is in the platform set).
        let result = negotiate(&input(Some("en-GB")));
        assert_eq!(
            result
                .fallback_chain
                .iter()
                .map(|locale| locale.as_str())
                .collect::<Vec<_>>(),
            vec!["en-GB", "en"]
        );
    }

    #[test]
    fn locale_set_requires_default_membership() {
        let set = LocaleSet::new(
            vec![Locale::parse("en").unwrap()],
            Locale::parse("ja").unwrap(),
        );
        assert!(matches!(set, Err(LocalizationError::SettingsConflict(_))));
    }
}
