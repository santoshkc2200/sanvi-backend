//! Privacy context configuration.

use sanvi_telemetry::Redacted;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PrivacyConfig {
    /// The KEK (key-encryption key) for envelope encryption, base64-encoded
    /// 32 bytes. Secrets come from the environment
    /// (`SANVI__PRIVACY__KEK`), never config files. Empty disables envelope
    /// encryption (export packaging and crypto-shredding refuse to run —
    /// fail closed).
    pub kek: Redacted<String>,
    #[serde(default)]
    pub disclosures: PrivacyDisclosures,
    #[serde(default)]
    pub enforce_release_checks: bool,
    #[serde(default)]
    pub erasure_ledger_dir: String,
    #[serde(default)]
    pub erasure_ledger_shared_and_durable: bool,
    /// KEK identifier recorded alongside wrapped DEKs (rotation adds a key,
    /// not a replacement).
    pub kek_id: String,

    /// Verification challenge lifetime and limits (seconds / attempts).
    pub verification_ttl_secs: i64,
    pub verification_max_attempts: i32,
    pub verification_otp_digits: u32,

    /// Export artifact policy.
    pub export_ttl_days: i64,
    pub export_max_downloads: i32,
    pub export_dir: String,

    /// The documented backup retention window: erased data may survive in
    /// backups for this long; the privacy notice publishes it.
    pub backup_retention_days: i64,

    /// The internal SLA (days) every DSR should complete within; alerting
    /// drives off this, not the legal maximum.
    pub internal_sla_days: i64,

    /// Retention sweep tuning. Dry-run is mandatory in production until the
    /// first verified run; the sweep reports, never silently deletes.
    pub retention_dry_run: bool,
    pub retention_sweep_batch_size: u32,

    /// Notice version served with consent records and at collection.
    pub notice_version: String,

    /// The GPC device cookie.
    pub device_cookie_name: String,
    pub device_cookie_ttl_days: i64,

    /// Sub-processor removal notice period (days) for tenant admins.
    pub subprocessor_notice_days: i64,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            kek: Redacted::default(),
            disclosures: PrivacyDisclosures::default(),
            enforce_release_checks: false,
            erasure_ledger_dir: String::new(),
            erasure_ledger_shared_and_durable: false,
            kek_id: "kek-1".to_string(),
            verification_ttl_secs: 600,
            verification_max_attempts: 5,
            verification_otp_digits: 6,
            export_ttl_days: 7,
            export_max_downloads: 3,
            export_dir: "./var/privacy-exports".to_string(),
            backup_retention_days: 30,
            internal_sla_days: 14,
            retention_dry_run: true,
            retention_sweep_batch_size: 500,
            notice_version: "2026-10-15".to_string(),
            device_cookie_name: "sanvi_gpc_device".to_string(),
            device_cookie_ttl_days: 30,
            subprocessor_notice_days: 30,
        }
    }
}

impl PrivacyConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.enforce_release_checks {
            if self.kek.is_empty() || self.retention_dry_run {
                anyhow::bail!(
                    "privacy release requires a configured KEK and verified active retention"
                );
            }
            if !std::path::Path::new(&self.erasure_ledger_dir).is_absolute()
                || !self.erasure_ledger_shared_and_durable
            {
                anyhow::bail!(
                    "privacy release requires an absolute shared durable erasure ledger outside the database backup boundary"
                );
            }
            self.disclosures.validate_for_publication()?;
        }
        if !self.kek.is_empty() {
            let decoded = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                self.kek.expose_secret(),
            )
            .map_err(|err| anyhow::anyhow!("privacy.kek is not valid base64: {err}"))?;
            if decoded.len() != 32 {
                anyhow::bail!("privacy.kek must decode to exactly 32 bytes (AES-256)");
            }
        }
        if self.kek_id.is_empty() {
            anyhow::bail!("privacy.kek_id must not be empty");
        }
        if self.verification_ttl_secs <= 0 {
            anyhow::bail!("privacy.verification_ttl_secs must be >= 1");
        }
        if self.verification_max_attempts < 1 || self.verification_max_attempts > 10 {
            anyhow::bail!("privacy.verification_max_attempts must be 1..=10");
        }
        if !(4..=8).contains(&self.verification_otp_digits) {
            anyhow::bail!("privacy.verification_otp_digits must be 4..=8");
        }
        if self.export_ttl_days <= 0 || self.export_ttl_days > 30 {
            anyhow::bail!("privacy.export_ttl_days must be 1..=30");
        }
        if self.export_max_downloads < 1 {
            anyhow::bail!("privacy.export_max_downloads must be >= 1");
        }
        if self.export_dir.is_empty() {
            anyhow::bail!("privacy.export_dir must not be empty");
        }
        if self.backup_retention_days <= 0 {
            anyhow::bail!("privacy.backup_retention_days must be >= 1");
        }
        if self.internal_sla_days <= 0 {
            anyhow::bail!("privacy.internal_sla_days must be >= 1");
        }
        if self.notice_version.is_empty() {
            anyhow::bail!("privacy.notice_version must not be empty");
        }
        if self.device_cookie_name.is_empty()
            || !self
                .device_cookie_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            anyhow::bail!("privacy.device_cookie_name must be alphanumeric/underscore");
        }
        if self.device_cookie_ttl_days <= 0 {
            anyhow::bail!("privacy.device_cookie_ttl_days must be >= 1");
        }
        if self.subprocessor_notice_days <= 0 {
            anyhow::bail!("privacy.subprocessor_notice_days must be >= 1");
        }
        Ok(())
    }

    /// Whether envelope encryption is usable in this configuration.
    pub fn encryption_enabled(&self) -> bool {
        !self.kek.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        PrivacyConfig::default().validate().unwrap();
    }

    #[test]
    fn a_32_byte_kek_is_accepted() {
        let key = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [7u8; 32]);
        let config = PrivacyConfig {
            kek: Redacted::new(key),
            ..PrivacyConfig::default()
        };
        config.validate().unwrap();
        assert!(config.encryption_enabled());
    }

    #[test]
    fn rejects_short_or_garbled_keks() {
        let config = PrivacyConfig {
            kek: Redacted::new("not base64 !!".to_string()),
            ..PrivacyConfig::default()
        };
        assert!(config.validate().is_err());

        let config = PrivacyConfig {
            kek: Redacted::new(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                [1u8; 16],
            )),
            ..PrivacyConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn production_release_rejects_draft_disclosures_and_missing_shared_ledger() {
        let mut config = PrivacyConfig {
            enforce_release_checks: true,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config.kek = Redacted::new(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            [7_u8; 32],
        ));
        config.retention_dry_run = false;
        assert!(config.validate().is_err());
        config.erasure_ledger_dir = "/srv/erasure-ledger".to_string();
        config.erasure_ledger_shared_and_durable = true;
        assert!(config.validate().is_err());
        config.disclosures = PrivacyDisclosures {
            controller_name: "Test legal entity".to_string(),
            business_address: "Test registered address".to_string(),
            privacy_email: "privacy@test.invalid".to_string(),
            hosting_countries: vec!["US".to_string()],
            transfer_safeguards: "Test documented safeguards".to_string(),
            reviewed_for_publication: true,
            ..Default::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn rejects_bad_cookie_names() {
        let config = PrivacyConfig {
            device_cookie_name: "bad name!".to_string(),
            ..PrivacyConfig::default()
        };
        assert!(config.validate().is_err());
    }
}

/// Organization details are deployment configuration; defaults are a clearly marked draft.
#[derive(Debug, Clone, Deserialize, Serialize, utoipa::ToSchema)]
#[serde(default)]
pub struct PrivacyDisclosures {
    pub controller_name: String,
    pub business_address: String,
    pub privacy_email: String,
    pub hosting_countries: Vec<String>,
    pub eu_representative: Option<String>,
    pub uk_representative: Option<String>,
    pub transfer_safeguards: String,
    pub reviewed_for_publication: bool,
    pub targeted_advertising: bool,
    pub sale_or_share: bool,
}
impl Default for PrivacyDisclosures {
    fn default() -> Self {
        Self {
            controller_name: "Sanvi (draft; legal entity not yet supplied)".to_string(),
            business_address: String::new(),
            privacy_email: String::new(),
            hosting_countries: Vec::new(),
            eu_representative: None,
            uk_representative: None,
            transfer_safeguards: String::new(),
            reviewed_for_publication: false,
            targeted_advertising: false,
            sale_or_share: false,
        }
    }
}
impl PrivacyDisclosures {
    pub fn validate_for_publication(&self) -> anyhow::Result<()> {
        if !self.reviewed_for_publication
            || self.controller_name.trim().is_empty()
            || self.controller_name.contains("draft")
            || self.business_address.trim().is_empty()
            || !self.privacy_email.contains('@')
            || self.privacy_email.ends_with("example.com")
            || self.hosting_countries.is_empty()
            || self.transfer_safeguards.trim().is_empty()
        {
            anyhow::bail!(
                "privacy.disclosures requires reviewed real controller, address, contact, hosting countries and transfer safeguards"
            );
        }
        Ok(())
    }
}
