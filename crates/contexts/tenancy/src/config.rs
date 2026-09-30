//! Tenancy configuration: subdomain base, resolution cache TTL and the
//! rotating internal-header signing keys.

use std::collections::BTreeMap;

use sanvi_telemetry::Redacted;
use serde::{Deserialize, Serialize};

use crate::adapters::resolution::SigningKey;

/// Tenancy section of the application configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TenancyConfig {
    /// Suffix under which tenant subdomains live: `acme.platform.sanvi.app`.
    /// Local development overrides this to `localhost`.
    pub base_domain: String,
    /// TTL of the host→tenant resolution cache.
    pub resolution_cache_ttl_secs: u64,
    /// Internal-header signing keys: key id → secret. A map (not a list) so
    /// `SANVI__TENANCY__SIGNING_KEYS__K1=…` environment overrides work.
    /// Keys with an empty secret are ignored (channel disabled per key).
    #[serde(default)]
    pub signing_keys: Redacted<BTreeMap<String, String>>,
    /// Which key id signs newly minted tokens (newest key first during
    /// rotation; verification accepts every key with a secret).
    #[serde(default)]
    pub active_key_id: String,
}

impl Default for TenancyConfig {
    fn default() -> Self {
        Self {
            base_domain: "platform.sanvi.app".to_string(),
            resolution_cache_ttl_secs: 60,
            signing_keys: Redacted::default(),
            active_key_id: String::new(),
        }
    }
}

impl TenancyConfig {
    /// Fail fast with *all* configuration problems at once.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems: Vec<String> = Vec::new();
        let base = self.base_domain.trim();
        if base.is_empty()
            || base.starts_with('.')
            || base.contains("://")
            || base.contains('/')
            || base.contains(':')
        {
            problems.push("tenancy.base_domain must be a bare domain name".to_string());
        }
        if self.resolution_cache_ttl_secs == 0 {
            problems.push("tenancy.resolution_cache_ttl_secs must be >= 1".to_string());
        }
        for (id, secret) in self.signing_keys.expose_secret() {
            if id.is_empty() || id.len() > 64 {
                problems.push("tenancy.signing_keys keys must be 1..=64 characters".to_string());
            }
            if !secret.is_empty() && secret.len() < 16 {
                problems.push(format!(
                    "tenancy.signing_keys[{id}] secret must be at least 16 characters"
                ));
            }
        }
        if !self.active_key_id.is_empty()
            && !self
                .signing_keys
                .expose_secret()
                .contains_key(&self.active_key_id)
        {
            problems.push(format!(
                "tenancy.active_key_id '{}' is not in tenancy.signing_keys",
                self.active_key_id
            ));
        }
        if !problems.is_empty() {
            anyhow::bail!(
                "invalid tenancy configuration:\n  - {}",
                problems.join("\n  - ")
            );
        }
        Ok(())
    }

    /// Keys that can actually sign/verify, active (minting) key first:
    /// empty secrets mean the operator has not (yet) configured the channel.
    pub fn usable_keys(&self) -> Vec<SigningKey> {
        let mut keys: Vec<SigningKey> = self
            .signing_keys
            .expose_secret()
            .iter()
            .filter(|(_, secret)| !secret.is_empty())
            .map(|(id, secret)| SigningKey {
                id: id.clone(),
                secret: Redacted::new(secret.clone()),
            })
            .collect();
        // Stable order: active key first, then the rest by id (rotation
        // accepts any key; the active key is the one new tokens are minted
        // with).
        keys.sort_by_key(|key| (key.id != self.active_key_id, key.id.clone()));
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        TenancyConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_hostile_base_domains() {
        for bad in ["", ".x.com", "https://x.com", "a/b", "a:8080"] {
            let config = TenancyConfig {
                base_domain: bad.to_string(),
                ..TenancyConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn empty_secrets_disable_the_internal_header() {
        let config = TenancyConfig {
            signing_keys: Redacted::new([("k1".to_string(), String::new())].into()),
            active_key_id: "k1".to_string(),
            ..TenancyConfig::default()
        };
        assert!(config.usable_keys().is_empty());
    }

    #[test]
    fn active_key_sorts_first_for_minting() {
        let config = TenancyConfig {
            signing_keys: Redacted::new(
                [
                    ("k1".to_string(), "old-secret-value".to_string()),
                    ("k2".to_string(), "new-secret-value".to_string()),
                ]
                .into(),
            ),
            active_key_id: "k2".to_string(),
            ..TenancyConfig::default()
        };
        let keys = config.usable_keys();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, "k2");
    }

    #[test]
    fn active_key_must_exist() {
        let config = TenancyConfig {
            signing_keys: Redacted::new(
                [("k1".to_string(), "a-long-enough-secret".to_string())].into(),
            ),
            active_key_id: "ghost".to_string(),
            ..TenancyConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
