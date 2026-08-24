//! Configuration declarations for later platform adapters. Values are loaded
//! from environment overrides in deployed environments and must never appear
//! in API responses, diagnostics, or `Debug` output.

use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AdvertisingConfig {
    pub google_ads_developer_token: String,
    pub google_ads_client_id: String,
    pub google_ads_client_secret: String,
    pub meta_client_id: String,
    pub meta_client_secret: String,
    pub oauth_state_signing_secret: String,
    pub tracking_site_key_signing_secret: String,
    pub google_ads_api_version: String,
    pub meta_api_version: String,
    /// Platform-wide cap on in-flight adapter calls (the shared
    /// developer-token quota). See `adapters::rate_limit`.
    #[serde(default = "default_quota_platform_wide_permits")]
    pub quota_platform_wide_permits: usize,
    /// Per-connection cap nested inside the platform-wide one, so one
    /// tenant's bulk edit cannot claim every platform-wide slot.
    #[serde(default = "default_quota_per_connection_permits")]
    pub quota_per_connection_permits: usize,
}

const fn default_quota_platform_wide_permits() -> usize {
    20
}

const fn default_quota_per_connection_permits() -> usize {
    3
}

impl std::fmt::Debug for AdvertisingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvertisingConfig")
            .field("google_ads_developer_token", &"[REDACTED]")
            .field("google_ads_client_id", &"[REDACTED]")
            .field("google_ads_client_secret", &"[REDACTED]")
            .field("meta_client_id", &"[REDACTED]")
            .field("meta_client_secret", &"[REDACTED]")
            .field("oauth_state_signing_secret", &"[REDACTED]")
            .field("tracking_site_key_signing_secret", &"[REDACTED]")
            .field("google_ads_api_version", &self.google_ads_api_version)
            .field("meta_api_version", &self.meta_api_version)
            .field(
                "quota_platform_wide_permits",
                &self.quota_platform_wide_permits,
            )
            .field(
                "quota_per_connection_permits",
                &self.quota_per_connection_permits,
            )
            .finish()
    }
}

impl Default for AdvertisingConfig {
    fn default() -> Self {
        Self {
            google_ads_developer_token: String::new(),
            google_ads_client_id: String::new(),
            google_ads_client_secret: String::new(),
            meta_client_id: String::new(),
            meta_client_secret: String::new(),
            oauth_state_signing_secret: String::new(),
            tracking_site_key_signing_secret: String::new(),
            google_ads_api_version: "v22".to_string(),
            meta_api_version: "v24.0".to_string(),
            quota_platform_wide_permits: default_quota_platform_wide_permits(),
            quota_per_connection_permits: default_quota_per_connection_permits(),
        }
    }
}

impl AdvertisingConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.google_ads_api_version.trim().is_empty() || self.meta_api_version.trim().is_empty()
        {
            anyhow::bail!("advertising API versions must not be empty");
        }
        if self.quota_platform_wide_permits == 0 || self.quota_per_connection_permits == 0 {
            anyhow::bail!("advertising quota permits must be at least 1");
        }
        if self.quota_per_connection_permits > self.quota_platform_wide_permits {
            anyhow::bail!(
                "advertising per-connection quota permits cannot exceed the platform-wide budget"
            );
        }
        Ok(())
    }

    pub fn validate_oauth_enabled(&self) -> anyhow::Result<()> {
        if self.oauth_state_signing_secret.len() < 32 {
            anyhow::bail!("advertising OAuth state signing secret must be at least 32 bytes");
        }
        Ok(())
    }
}
