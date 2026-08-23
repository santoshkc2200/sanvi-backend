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
    pub tracking_site_key_signing_secret: String,
    pub google_ads_api_version: String,
    pub meta_api_version: String,
}

impl std::fmt::Debug for AdvertisingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvertisingConfig")
            .field("google_ads_developer_token", &"[REDACTED]")
            .field("google_ads_client_id", &"[REDACTED]")
            .field("google_ads_client_secret", &"[REDACTED]")
            .field("meta_client_id", &"[REDACTED]")
            .field("meta_client_secret", &"[REDACTED]")
            .field("tracking_site_key_signing_secret", &"[REDACTED]")
            .field("google_ads_api_version", &self.google_ads_api_version)
            .field("meta_api_version", &self.meta_api_version)
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
            tracking_site_key_signing_secret: String::new(),
            google_ads_api_version: "v22".to_string(),
            meta_api_version: "v24.0".to_string(),
        }
    }
}

impl AdvertisingConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.google_ads_api_version.trim().is_empty() || self.meta_api_version.trim().is_empty()
        {
            anyhow::bail!("advertising API versions must not be empty");
        }
        Ok(())
    }
}
