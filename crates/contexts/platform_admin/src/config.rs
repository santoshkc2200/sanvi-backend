//! Platform admin context configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PlatformAdminConfig {
    /// Hard ceiling on impersonation grant duration (the plan's 30 minutes).
    pub impersonation_max_minutes: u32,
    /// How recent an `aal2` authentication must be for dangerous actions
    /// (fresh-MFA extractor policy).
    pub fresh_aal2_max_age_secs: i64,
}

impl Default for PlatformAdminConfig {
    fn default() -> Self {
        Self {
            impersonation_max_minutes: 30,
            fresh_aal2_max_age_secs: 300,
        }
    }
}

impl PlatformAdminConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.impersonation_max_minutes == 0 || self.impersonation_max_minutes > 60 {
            anyhow::bail!("platform_admin.impersonation_max_minutes must be 1..=60");
        }
        if self.fresh_aal2_max_age_secs <= 0 {
            anyhow::bail!("platform_admin.fresh_aal2_max_age_secs must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        PlatformAdminConfig::default().validate().unwrap();
    }

    #[test]
    fn impersonation_is_capped_by_the_plan() {
        let config = PlatformAdminConfig {
            impersonation_max_minutes: 90,
            ..PlatformAdminConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
