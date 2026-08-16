//! Access context configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AccessConfig {
    /// TTL of cached authorization decisions. Bounded at 30 s so a revoked
    /// role removes access within the acceptance window without a restart.
    pub decision_cache_ttl_secs: u64,
    /// Maximum decisions the local cache retains.
    pub decision_cache_max_entries: usize,
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            decision_cache_ttl_secs: 30,
            decision_cache_max_entries: 10_000,
        }
    }
}

impl AccessConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.decision_cache_ttl_secs == 0 || self.decision_cache_ttl_secs > 30 {
            anyhow::bail!("access.decision_cache_ttl_secs must be 1..=30");
        }
        if self.decision_cache_max_entries == 0 {
            anyhow::bail!("access.decision_cache_max_entries must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        AccessConfig::default().validate().unwrap();
    }

    #[test]
    fn cache_ttl_is_bounded_by_the_plan() {
        let config = AccessConfig {
            decision_cache_ttl_secs: 45,
            ..AccessConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
