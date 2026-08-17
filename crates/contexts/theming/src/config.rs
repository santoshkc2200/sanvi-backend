//! Theming context configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ThemingConfig {
    /// TTL for the resolved-theme cache (Redis with a process-local
    /// fallback), seconds. Publish events invalidate immediately; the TTL
    /// is the staleness bound when an invalidation event is lost.
    pub resolved_cache_ttl_secs: u64,

    /// Cap on the sanitised custom CSS a tenant may store, bytes.
    pub custom_css_max_bytes: usize,

    /// Cap on one brand asset upload, bytes.
    pub asset_max_bytes: usize,

    /// Base URL prefix for resolved asset links (theme artifacts and brand
    /// assets), e.g. `https://assets.sanvi.example`. Path suffix is added
    /// by the resolver.
    pub asset_base_url: String,

    /// Local directory where uploaded brand assets and published artifacts
    /// are written (dev/self-hosted storage; production points a CDN or
    /// object store at the same layout).
    pub artifact_dir: String,

    /// TTL for signed draft-preview tokens, seconds.
    pub preview_token_ttl_secs: u64,

    /// HMAC secret for preview tokens (base64). Without a secret, preview
    /// token signing fails closed.
    pub preview_token_secret: String,
}

impl Default for ThemingConfig {
    fn default() -> Self {
        Self {
            resolved_cache_ttl_secs: 86_400,
            custom_css_max_bytes: 8_192,
            asset_max_bytes: 2 * 1024 * 1024,
            asset_base_url: "/assets/theming".to_string(),
            artifact_dir: "artifacts/theming".to_string(),
            preview_token_ttl_secs: 300,
            preview_token_secret: String::new(),
        }
    }
}

impl ThemingConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.resolved_cache_ttl_secs == 0 {
            anyhow::bail!("theming.resolved_cache_ttl_secs must be >= 1");
        }
        if self.custom_css_max_bytes < 256 {
            anyhow::bail!("theming.custom_css_max_bytes must be >= 256");
        }
        if self.asset_max_bytes == 0 {
            anyhow::bail!("theming.asset_max_bytes must be >= 1");
        }
        if !self.asset_base_url.starts_with('/') && !self.asset_base_url.starts_with("https://") {
            anyhow::bail!(
                "theming.asset_base_url must be an absolute path or https URL (got {})",
                self.asset_base_url
            );
        }
        if self.artifact_dir.trim().is_empty() {
            anyhow::bail!("theming.artifact_dir must not be empty");
        }
        if self.preview_token_ttl_secs == 0 || self.preview_token_ttl_secs > 3600 {
            anyhow::bail!("theming.preview_token_ttl_secs must be in 1..=3600");
        }
        // An empty secret is valid: preview signing fails closed at runtime
        // (the endpoint returns 503 until a secret is configured). A secret
        // that is set must be base64 and at least 32 bytes.
        if !self.preview_token_secret.is_empty()
            && !base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                &self.preview_token_secret,
            )
            .map(|bytes| bytes.len() >= 32)
            .unwrap_or(false)
        {
            anyhow::bail!(
                "theming.preview_token_secret must be base64-encoded and at least 32 bytes"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        ThemingConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_weak_secrets_and_bad_urls() {
        let config = ThemingConfig {
            preview_token_secret: "short".to_string(),
            ..ThemingConfig::default()
        };
        assert!(config.validate().is_err());

        let config = ThemingConfig {
            asset_base_url: "http://assets.example".to_string(),
            ..ThemingConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
