//! Audit context configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AuditConfig {
    /// Batch size of the chain verifier job (rows per page).
    pub verify_batch_size: u32,
    /// Retention guarantee in months. Rows are never deletable by the app
    /// role (append-only grants + trigger); archival to cold storage is a
    /// migrator-role maintenance procedure documented in
    /// docs/runbooks/audit-chain.md.
    pub retention_months: u32,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            verify_batch_size: 1000,
            retention_months: 12,
        }
    }
}

impl AuditConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.verify_batch_size == 0 {
            anyhow::bail!("audit.verify_batch_size must be >= 1");
        }
        if self.retention_months < 12 {
            anyhow::bail!("audit.retention_months must be >= 12 (the compliance floor)");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        AuditConfig::default().validate().unwrap();
    }

    #[test]
    fn retention_below_the_floor_is_rejected() {
        let config = AuditConfig {
            retention_months: 6,
            ..AuditConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
