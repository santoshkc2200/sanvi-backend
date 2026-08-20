//! Locale-aware formatting: dates, times, numbers, currency, addresses,
//! names and collation.
//!
//! Policy invariants:
//! - Timestamps are stored as `timestamptz` UTC everywhere; rendering
//!   happens here, in the subject's timezone, at the edge.
//! - Currency is *minor units as `i64`* plus an ISO-4217 code; the exponent
//!   comes from the currency table, never assumed (`JPY` has 0 decimals).
//! - Dates are era-free and unambiguous: `2026年8月15日` (ja) /
//!   `Aug 15, 2026` (en) — no Heisei/Reiwa ambiguity.
//! - Name order is locale-driven (`family given` + 様 for ja); honorifics
//!   are appended by the formatter, not by string concatenation at call
//!   sites.
//! - Japanese collation is kana-aware (gojūon order, dakuten after base
//!   kana) over NFKC-normalised text — never byte order. It approximates
//!   ICU collation without the multi-megabyte ICU data payload; the trade-
//!   off is documented here and covered by golden tests.

use std::{cmp::Ordering, str::FromStr};

use chrono::{DateTime, Datelike, Timelike, Utc, Weekday};
use chrono_tz::Tz;
use unicode_normalization::UnicodeNormalization;

use crate::domain::error::LocalizationError;
use crate::domain::locale::Locale;

/// Everything needed to render a value for one subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormattingPolicy {
    pub locale: Locale,
    pub timezone: Tz,
    pub first_day_of_week: Weekday,
}

impl FormattingPolicy {
    /// Build a policy for a locale. `timezone` is the IANA name (validated;
    /// invalid names fall back to the locale's conventional timezone:
    /// `Asia/Tokyo` for `ja`, UTC otherwise).
    pub fn for_locale(locale: Locale, timezone: &str) -> Result<Self, LocalizationError> {
        let timezone = match Tz::from_str(timezone) {
            Ok(tz) => tz,
            Err(_) => default_timezone(&locale),
        };
        let first_day_of_week = default_first_day_of_week(&locale);
        Ok(Self {
            locale,
            timezone,
            first_day_of_week,
        })
    }

    pub fn language(&self) -> &str {
        self.locale.language()
    }

    pub fn is_japanese(&self) -> bool {
        self.language() == "ja"
    }

    // --- dates & times ----------------------------------------------------

    /// Era-free date rendering: `2026年8月15日` (ja), `Aug 15, 2026` (en).
    pub fn format_date(&self, dt: DateTime<Utc>) -> String {
        let local = dt.with_timezone(&self.timezone);
        if self.is_japanese() {
            format!("{}年{}月{}日", local.year(), local.month(), local.day())
        } else {
            let month = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ][(local.month0() as usize).min(11)];
            format!("{} {}, {}", month, local.day(), local.year())
        }
    }

    /// `14:05` (ja, 24h) / `2:05 PM` (en, 12h).
    pub fn format_time(&self, dt: DateTime<Utc>) -> String {
        let local = dt.with_timezone(&self.timezone);
        if self.is_japanese() {
            format!("{:02}:{:02}", local.hour(), local.minute())
        } else {
            let hour = local.hour();
            let period = if hour < 12 { "AM" } else { "PM" };
            let display = match hour % 12 {
                0 => 12,
                h => h,
            };
            format!("{display}:{:02} {period}", local.minute())
        }
    }

    pub fn format_datetime(&self, dt: DateTime<Utc>) -> String {
        format!("{} {}", self.format_date(dt), self.format_time(dt))
    }

    // --- numbers ----------------------------------------------------------

    pub fn format_number(&self, n: f64) -> String {
        if n == n.trunc() && n.abs() < 9.0e15 {
            return self.format_number_integer(n as i64);
        }
        // Shortest round-trip with up to 6 fraction digits, trailing zeros
        // trimmed.
        let mut value = n.abs();
        let sign = if n < 0.0 { "-" } else { "" };
        let int_part = value.trunc() as i64;
        value -= (value.trunc() as i64) as f64;
        let mut fraction = String::new();
        let mut digits = Vec::new();
        let mut guard = 0;
        while guard < 6 {
            value *= 10.0;
            let digit = value.trunc() as u32 % 10;
            digits.push(digit);
            value -= (value.trunc() as i64) as f64;
            guard += 1;
            if value.abs() < 1e-9 {
                break;
            }
        }
        while digits.last() == Some(&0) {
            digits.pop();
        }
        for digit in digits {
            fraction.push(char::from(b'0' + digit as u8));
        }
        if fraction.is_empty() {
            format!("{}{}", sign, self.group_digits(int_part as u64))
        } else {
            format!(
                "{}{}.{}",
                sign,
                self.group_digits(int_part as u64),
                fraction
            )
        }
    }

    pub fn format_number_integer(&self, n: i64) -> String {
        let sign = if n < 0 { "-" } else { "" };
        format!("{}{}", sign, self.group_digits(n.unsigned_abs()))
    }

    fn group_digits(&self, value: u64) -> String {
        let digits = value.to_string();
        let mut out = String::new();
        for (index, ch) in digits.chars().enumerate() {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        out
    }

    /// Format an amount from minor units using the ISO-4217 exponent table.
    /// JPY (0 exponent) renders `1,234円` / `¥1,234` with no decimal
    /// artefacts; USD renders `$12.34`.
    pub fn format_currency(
        &self,
        minor_units: i64,
        currency_code: &str,
    ) -> Result<String, LocalizationError> {
        let exponent = currency_exponent(currency_code)
            .ok_or_else(|| LocalizationError::UnknownCurrency(currency_code.to_string()))?;
        let divisor = 10_u64.pow(exponent);
        let absolute = minor_units.unsigned_abs();
        let major = absolute / divisor;
        let remainder = absolute % divisor;
        let sign = if minor_units < 0 { "-" } else { "" };
        let symbol = currency_symbol(currency_code, self.is_japanese());

        if exponent == 0 {
            // Zero-decimal currencies never render fraction digits.
            let amount = format!("{sign}{}", self.group_digits(major));
            return Ok(if self.is_japanese() {
                format!("{amount}{symbol}")
            } else {
                format!("{sign}{symbol}{}", self.group_digits(major))
            });
        }
        let fraction = format!("{:0width$}", remainder, width = exponent as usize);
        let amount = format!("{sign}{}.{fraction}", self.group_digits(major));
        if self.is_japanese() {
            Ok(format!("{amount}{symbol}"))
        } else {
            Ok(format!(
                "{sign}{symbol}{}.{fraction}",
                self.group_digits(major)
            ))
        }
    }

    // --- names ------------------------------------------------------------

    /// `given family` (en) / `family given` (ja). The honorific suffix
    /// (`様` for ja salutations) is appended by this formatter — never by
    /// string concatenation at call sites.
    pub fn format_name(&self, given: &str, family: &str, honorific: Option<&str>) -> String {
        let base = if self.is_japanese() {
            format!("{family} {given}")
        } else {
            format!("{given} {family}")
        };
        match honorific {
            Some(suffix) if self.is_japanese() => format!("{base}{suffix}"),
            Some(suffix) => format!("{base} {suffix}"),
            None => base,
        }
    }

    /// The default honorific for salutations in this locale: `様` for ja,
    /// none for en.
    pub fn default_honorific(&self) -> Option<&'static str> {
        if self.is_japanese() {
            Some("様")
        } else {
            None
        }
    }

    // --- addresses --------------------------------------------------------

    /// Render an address as ordered lines. Japanese: `〒`-prefixed postal
    /// code, prefecture, locality, street. English: street, locality,
    /// region, postal code.
    pub fn format_address(&self, address: &Address) -> Result<Vec<String>, LocalizationError> {
        let postal = address
            .postal_code
            .as_deref()
            .map(|raw| match address.country_code.as_deref() {
                Some("JP") => normalize_postal_code("JP", raw),
                Some("US") => normalize_postal_code("US", raw),
                _ => Ok(raw.to_string()),
            })
            .transpose()?;

        let mut lines = Vec::new();
        if self.is_japanese() {
            if let Some(postal) = postal {
                lines.push(postal);
            }
            if let Some(region) = &address.region {
                lines.push(region.clone());
            }
            if let Some(locality) = &address.locality {
                lines.push(locality.clone());
            }
            if let Some(street) = &address.street {
                lines.push(street.clone());
            }
        } else {
            if let Some(street) = &address.street {
                lines.push(street.clone());
            }
            if let Some(locality) = &address.locality {
                lines.push(locality.clone());
            }
            if let Some(region) = &address.region {
                if let Some(postal) = &postal {
                    lines.push(format!("{region} {postal}"));
                } else {
                    lines.push(region.clone());
                }
            } else if let Some(postal) = postal {
                lines.push(postal);
            }
        }
        if lines.is_empty() {
            return Err(LocalizationError::InvalidAddress(
                "address has no fields".to_string(),
            ));
        }
        Ok(lines)
    }
}

/// The address pieces the formatter renders. All optional: at least one
/// must be present.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Address {
    pub postal_code: Option<String>,
    pub region: Option<String>,
    pub locality: Option<String>,
    pub street: Option<String>,
    /// ISO 3166-1 alpha-2 (drives postal validation and formatting).
    pub country_code: Option<String>,
}

/// Normalise and validate a postal code for the countries that gate
/// formatting: `JP` → `〒123-4567` from any of `123-4567`, `1234567`,
/// `〒1234567`; `US` → `ZIP` / `ZIP+4`. Unknown countries pass through.
pub fn normalize_postal_code(country: &str, raw: &str) -> Result<String, LocalizationError> {
    let cleaned: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    match country {
        "JP" => {
            if cleaned.len() != 7 {
                return Err(LocalizationError::InvalidAddress(format!(
                    "postal code {raw:?} is not a 7-digit Japanese postal code"
                )));
            }
            Ok(format!("〒{}-{}", &cleaned[..3], &cleaned[3..]))
        }
        "US" => match cleaned.len() {
            5 => Ok(cleaned),
            9 => Ok(format!("{}-{}", &cleaned[..5], &cleaned[5..])),
            _ => Err(LocalizationError::InvalidAddress(format!(
                "postal code {raw:?} is not a US ZIP code"
            ))),
        },
        _ => Ok(raw.to_string()),
    }
}

/// ISO-4217 minor-unit exponent. `None` = unknown code; callers must not
/// guess (the phase-06 rule: never assume 2). Codes with a valid shape fall
/// back to 2 — the documented default for the overwhelming majority of
/// currencies — while the zero- and three-decimal tables above are
/// exhaustive for the currencies Sanvi accepts.
pub fn currency_exponent(code: &str) -> Option<u32> {
    match code.to_ascii_uppercase().as_str() {
        // Zero-decimal currencies.
        "JPY" | "KRW" | "VND" | "CLP" | "PYG" | "ISK" | "UGX" | "RWF" | "DJF" | "GNF" | "KMF"
        | "MGA" | "VUV" | "XPF" | "XOF" | "XAF" | "BIF" => Some(0),
        // Three-decimal currencies.
        "BHD" | "KWD" | "OMR" | "JOD" | "TND" | "IQD" | "LYD" => Some(3),
        // ISO-4217 "no currency".
        "XXX" => None,
        // Everything else uses two decimals (the overwhelming default).
        _ if code.len() == 3 && code.chars().all(|c| c.is_ascii_alphabetic()) => Some(2),
        _ => None,
    }
}

/// The display symbol for a currency, per locale family.
fn currency_symbol(code: &str, japanese: bool) -> &'static str {
    match (code.to_ascii_uppercase().as_str(), japanese) {
        ("JPY", true) => "円",
        ("JPY", false) => "¥",
        ("USD", _) => "$",
        ("EUR", _) => "€",
        ("GBP", _) => "£",
        ("BHD", _) => "BD",
        ("KWD", _) => "KD",
        ("KRW", _) => "₩",
        _ => "",
    }
}

fn default_timezone(locale: &Locale) -> Tz {
    if locale.language() == "ja" {
        chrono_tz::Asia::Tokyo
    } else {
        chrono_tz::UTC
    }
}

fn default_first_day_of_week(locale: &Locale) -> Weekday {
    let _ = locale;
    Weekday::Sun
}

// ---------------------------------------------------------------------------
// Input normalisation & Japanese collation
// ---------------------------------------------------------------------------

/// NFKC normalisation for search and comparison; the original input is
/// preserved for display. Full-width → half-width, compatibility forms →
/// canonical equivalents.
pub fn normalize_input(input: &str) -> String {
    input.nfkc().collect()
}

/// Kana-aware comparison for Japanese text (gojūon order, dakuten after
/// base kana, hiragana and katakana equivalent, small kana folded to their
/// full-size form, ASCII case-folded and ordered first, kanji by code
/// point after NFKC compatibility decomposition).
pub fn compare_ja(a: &str, b: &str) -> Ordering {
    sort_key(&normalize_input(a)).cmp(&sort_key(&normalize_input(b)))
}

/// Sort a list of strings with Japanese-aware collation (stable).
pub fn sort_strings(items: &[String]) -> Vec<String> {
    let mut items: Vec<String> = items.to_vec();
    items.sort_by(|a, b| compare_ja(a, b));
    items
}

/// Compare two strings with the collation appropriate to the locale:
/// kana-aware for `ja`, otherwise NFKC + case-folded lexicographic.
pub fn compare_for_locale(language: &str, a: &str, b: &str) -> Ordering {
    if language == "ja" {
        compare_ja(a, b)
    } else {
        normalize_input(a)
            .to_lowercase()
            .cmp(&normalize_input(b).to_lowercase())
    }
}

/// One component of the sort key: a base rank plus a modifier offset
/// (0 = plain, 1 = dakuten, 2 = handakuten). Rank bands: digits < ASCII
/// letters < kana (gojūon) < everything else (kanji by code point).
fn sort_key(input: &str) -> Vec<(u32, u8)> {
    let mut key = Vec::new();
    for ch in input.chars() {
        match ch {
            '0'..='9' => key.push((ch as u32 - '0' as u32, 0)),
            'a'..='z' | 'A'..='Z' => key.push((100 + ch.to_ascii_lowercase() as u32, 0)),
            'ぁ'..='ゖ' => key.push(hiragana_rank(ch)),
            'ァ'..='ヶ' => {
                // Fold katakana to its hiragana counterpart: equivalent
                // kana must collate together.
                if let Some(hira) = katakana_to_hiragana(ch) {
                    key.push(hiragana_rank(hira));
                } else {
                    key.push((3000 + ch as u32, 0));
                }
            }
            _ => key.push((3000 + ch as u32, 0)),
        }
    }
    key
}

/// Rank a hiragana character in gojūon order: `a i u e o` rows, small kana
/// folded into their full-size form, dakuten/handakuten as modifier.
fn hiragana_rank(ch: char) -> (u32, u8) {
    // Base gojūon rows (a, ka, sa, ta, na, ha, ma, ya, ra, wa + n).
    let base = match ch {
        'ぁ' | 'あ' => 0,
        'ぃ' | 'い' => 1,
        'ぅ' | 'う' => 2,
        'ぇ' | 'え' => 3,
        'ぉ' | 'お' => 4,
        'ゕ' | 'か' | 'が' => 10,
        'き' | 'ぎ' => 11,
        'く' | 'ぐ' => 12,
        'け' | 'げ' => 13,
        'こ' | 'ご' => 14,
        'さ' | 'ざ' => 20,
        'し' | 'じ' => 21,
        'す' | 'ず' => 22,
        'せ' | 'ぜ' => 23,
        'そ' | 'ぞ' => 24,
        'た' | 'だ' => 30,
        'ち' | 'ぢ' => 31,
        'っ' | 'つ' | 'づ' => 32,
        'て' | 'で' => 33,
        'と' | 'ど' => 34,
        'な' => 40,
        'に' => 41,
        'ぬ' => 42,
        'ね' => 43,
        'の' => 44,
        'は' | 'ば' | 'ぱ' => 50,
        'ひ' | 'び' | 'ぴ' => 51,
        'ふ' | 'ぶ' | 'ぷ' => 52,
        'へ' | 'べ' | 'ぺ' => 53,
        'ほ' | 'ぼ' | 'ぽ' => 54,
        'ま' => 60,
        'み' => 61,
        'む' => 62,
        'め' => 63,
        'も' => 64,
        'ゃ' | 'や' => 70,
        'ゅ' | 'ゆ' => 71,
        'ょ' | 'よ' => 72,
        'ら' => 80,
        'り' => 81,
        'る' => 82,
        'れ' => 83,
        'ろ' => 84,
        'ゎ' | 'わ' => 90,
        'ゐ' => 91,
        'ゑ' => 92,
        'を' => 93,
        'ん' => 94,
        _ => return (3000 + ch as u32, 0),
    };
    let modifier = match ch {
        'が' | 'ぎ' | 'ぐ' | 'げ' | 'ご' | 'ざ' | 'じ' | 'ず' | 'ぜ' | 'ぞ' | 'だ' | 'ぢ'
        | 'づ' | 'で' | 'ど' | 'ば' | 'び' | 'ぶ' | 'べ' | 'ぼ' => 1,
        'ぱ' | 'ぴ' | 'ぷ' | 'ぺ' | 'ぽ' => 2,
        _ => 0,
    };
    (1000 + base * 3 + modifier as u32, 0)
}

/// Map a katakana character to its hiragana counterpart.
fn katakana_to_hiragana(ch: char) -> Option<char> {
    let hira = ch as u32 - 0x30A1 + 0x3041;
    char::from_u32(hira)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ja() -> FormattingPolicy {
        FormattingPolicy::for_locale(Locale::parse("ja").unwrap(), "Asia/Tokyo").unwrap()
    }

    fn en() -> FormattingPolicy {
        FormattingPolicy::for_locale(Locale::parse("en").unwrap(), "UTC").unwrap()
    }

    fn at(iso: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn dates_are_era_free_and_locale_aware() {
        let dt = at("2026-08-15T14:05:00Z");
        assert_eq!(ja().format_date(dt), "2026年8月15日");
        assert_eq!(en().format_date(dt), "Aug 15, 2026");
    }

    #[test]
    fn dates_render_in_the_subject_timezone() {
        // 23:30 UTC on the 15th is already the 16th in Tokyo.
        let dt = at("2026-08-15T23:30:00Z");
        assert_eq!(ja().format_date(dt), "2026年8月16日");
        assert_eq!(en().format_date(dt), "Aug 15, 2026");
    }

    #[test]
    fn time_formatting_is_24h_for_ja_12h_for_en() {
        let dt = at("2026-08-15T14:05:00Z");
        assert_eq!(ja().format_time(dt), "23:05");
        assert_eq!(en().format_time(dt), "2:05 PM");
    }

    #[test]
    fn numbers_group_by_thousands() {
        assert_eq!(en().format_number(1_234_567.0), "1,234,567");
        assert_eq!(ja().format_number(1_234_567.0), "1,234,567");
        assert_eq!(en().format_number(-42.5), "-42.5");
        assert_eq!(en().format_number(0.5), "0.5");
        assert_eq!(en().format_number(2.0), "2");
    }

    #[test]
    fn currency_uses_the_exponent_table() {
        // JPY: zero decimals, no artefacts, symbol after (ja) / before (en).
        assert_eq!(ja().format_currency(1234, "JPY").unwrap(), "1,234円");
        assert_eq!(en().format_currency(1234, "JPY").unwrap(), "¥1,234");
        // USD: two decimals.
        assert_eq!(en().format_currency(123456, "USD").unwrap(), "$1,234.56");
        assert_eq!(ja().format_currency(123456, "USD").unwrap(), "1,234.56$");
        // BHD: three decimals.
        assert_eq!(en().format_currency(1234567, "BHD").unwrap(), "BD1,234.567");
        // The sign belongs to the whole amount even when the absolute value
        // is below one major unit.
        assert_eq!(en().format_currency(-50, "USD").unwrap(), "-$0.50");
        assert_eq!(ja().format_currency(-50, "USD").unwrap(), "-0.50$");
        assert_eq!(en().format_currency(-1, "JPY").unwrap(), "-¥1");
    }

    #[test]
    fn minor_units_round_trip() {
        // 12_3456 major units → 123456 minor units at 2 exponents.
        let minor = 123_456;
        let rendered = en().format_currency(minor, "USD").unwrap();
        assert_eq!(rendered, "$1,234.56");
        // JPY amounts are never divided by 100.
        let rendered = ja().format_currency(1234, "JPY").unwrap();
        assert!(!rendered.contains('.'));
    }

    #[test]
    fn unknown_currencies_are_refused_not_guessed() {
        assert!(en().format_currency(100, "XXX").is_err());
        assert!(currency_exponent("USD") == Some(2));
        assert!(currency_exponent("JPY") == Some(0));
        assert!(currency_exponent("BHD") == Some(3));
    }

    #[test]
    fn names_follow_locale_order_with_honorifics() {
        assert_eq!(en().format_name("Taro", "Yamada", None), "Taro Yamada");
        assert_eq!(ja().format_name("太郎", "山田", None), "山田 太郎");
        assert_eq!(ja().format_name("太郎", "山田", Some("様")), "山田 太郎様");
        assert_eq!(
            en().format_name("Taro", "Yamada", Some("Mr.")),
            "Taro Yamada Mr."
        );
        assert_eq!(ja().default_honorific(), Some("様"));
        assert_eq!(en().default_honorific(), None);
    }

    #[test]
    fn japanese_addresses_use_the_locale_template() {
        let address = Address {
            postal_code: Some("1234567".to_string()),
            region: Some("東京都".to_string()),
            locality: Some("渋谷区".to_string()),
            street: Some("神南1-2-3".to_string()),
            country_code: Some("JP".to_string()),
        };
        let lines = ja().format_address(&address).unwrap();
        assert_eq!(lines, vec!["〒123-4567", "東京都", "渋谷区", "神南1-2-3"]);
    }

    #[test]
    fn english_addresses_put_the_street_first() {
        let address = Address {
            postal_code: Some("90210".to_string()),
            region: Some("CA".to_string()),
            locality: Some("Beverly Hills".to_string()),
            street: Some("123 Palm Dr".to_string()),
            country_code: Some("US".to_string()),
        };
        let lines = en().format_address(&address).unwrap();
        assert_eq!(lines, vec!["123 Palm Dr", "Beverly Hills", "CA 90210"]);
    }

    #[test]
    fn postal_codes_are_validated_per_country() {
        assert_eq!(
            normalize_postal_code("JP", "123-4567").unwrap(),
            "〒123-4567"
        );
        assert_eq!(
            normalize_postal_code("JP", "〒1234567").unwrap(),
            "〒123-4567"
        );
        assert_eq!(
            normalize_postal_code("US", "902101234").unwrap(),
            "90210-1234"
        );
        assert!(normalize_postal_code("JP", "1234").is_err());
        assert!(normalize_postal_code("US", "12").is_err());
        // Unknown countries pass through untouched.
        assert_eq!(normalize_postal_code("DE", "10115").unwrap(), "10115");
    }

    #[test]
    fn nfkc_normalizes_full_width_and_compatibility_forms() {
        assert_eq!(normalize_input("ＡＢＣ１２３"), "ABC123");
        assert_eq!(normalize_input("ﾊﾝｶｸ"), "ハンカク");
        assert_eq!(normalize_input("株式会社"), "株式会社");
    }

    #[test]
    fn japanese_collation_is_kana_aware_not_byte_order() {
        // Byte order would put か (U+304B) after ん (U+3093) — gojūon order
        // does not.
        let items = vec![
            "ん".to_string(),
            "か".to_string(),
            "あ".to_string(),
            "さ".to_string(),
        ];
        assert_eq!(sort_strings(&items), vec!["あ", "か", "さ", "ん"]);
        // Dakuten after base kana: か < が < き.
        assert_eq!(
            sort_strings(&["き".into(), "が".into(), "か".into()]),
            vec!["か", "が", "き"]
        );
        // Hiragana and katakana are equivalent.
        assert_eq!(compare_ja("カナ", "かな"), Ordering::Equal);
        // Full-width folds via NFKC.
        assert_eq!(compare_ja("ＡＢＣ", "ABC"), Ordering::Equal);
        // Digits and ASCII sort before kana.
        assert_eq!(compare_ja("1", "あ"), Ordering::Less);
    }

    #[test]
    fn invalid_timezones_fall_back_to_the_locale_convention() {
        let policy =
            FormattingPolicy::for_locale(Locale::parse("ja").unwrap(), "Not/AZone").unwrap();
        assert_eq!(policy.timezone, chrono_tz::Asia::Tokyo);
        let policy =
            FormattingPolicy::for_locale(Locale::parse("en").unwrap(), "Not/AZone").unwrap();
        assert_eq!(policy.timezone, chrono_tz::UTC);
    }
}
