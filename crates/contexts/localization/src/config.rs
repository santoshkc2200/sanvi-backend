//! Localization context configuration.

use serde::{Deserialize, Serialize};

use crate::domain::locale::Locale;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct LocalizationConfig {
    /// The platform default locale (the last fallback for every tenant and
    /// user).
    pub default_locale: String,

    /// Allow-list for the explicit `?locale=` query parameter. Untrusted
    /// input; anything not listed is ignored by negotiation.
    pub explicit_locale_allowlist: Vec<String>,

    /// TTL for merged catalog snapshots (static + overrides), seconds.
    pub merge_cache_ttl_secs: u64,

    /// TTL for tenant locale settings used by negotiation, seconds.
    pub tenant_settings_cache_ttl_secs: u64,

    /// TTL for the signed-in user's locale preference lookup, seconds.
    pub user_locale_cache_ttl_secs: u64,
}

impl Default for LocalizationConfig {
    fn default() -> Self {
        Self {
            default_locale: "en".to_string(),
            explicit_locale_allowlist: vec!["en".to_string(), "ja".to_string()],
            merge_cache_ttl_secs: 30,
            tenant_settings_cache_ttl_secs: 30,
            user_locale_cache_ttl_secs: 60,
        }
    }
}

impl LocalizationConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Locale::parse(&self.default_locale)
            .map_err(|err| anyhow::anyhow!("localization.default_locale is invalid: {err}"))?;
        if self.explicit_locale_allowlist.is_empty() {
            anyhow::bail!("localization.explicit_locale_allowlist must not be empty");
        }
        for tag in &self.explicit_locale_allowlist {
            Locale::parse(tag).map_err(|err| {
                anyhow::anyhow!(
                    "localization.explicit_locale_allowlist contains an invalid tag: {err}"
                )
            })?;
        }
        if self.merge_cache_ttl_secs == 0
            || self.tenant_settings_cache_ttl_secs == 0
            || self.user_locale_cache_ttl_secs == 0
        {
            anyhow::bail!("localization cache TTLs must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        LocalizationConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_bad_locales_and_empty_allowlists() {
        let config = LocalizationConfig {
            default_locale: "not a locale!".to_string(),
            ..LocalizationConfig::default()
        };
        assert!(config.validate().is_err());

        let config = LocalizationConfig {
            explicit_locale_allowlist: vec![],
            ..LocalizationConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
