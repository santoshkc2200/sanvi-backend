//! Identity context configuration.

use serde::{Deserialize, Serialize};

/// Identity & session configuration.
///
/// Secrets (webhook secret, Kratos endpoints) come from the environment
/// (`SANVI__IDENTITY__*`), never from committed files.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct IdentityConfig {
    /// TTL of the verified-session cache (≤ 60 s keeps the Kratos logout
    /// webhook's invalidation cheap and correct).
    pub session_cache_ttl_secs: u64,
    /// TTL of an account-linking intent (the challenge window).
    pub link_intent_ttl_secs: u64,
    /// Invitation lifetime in days.
    pub invitation_ttl_days: i64,
    /// Name of the Kratos session cookie (and the bearer-compatible API
    /// token header we accept).
    pub session_cookie_name: String,
    /// Shared secret every Kratos webhook must present (constant-time
    /// compared). Empty disables the webhook endpoints entirely.
    pub webhook_secret: String,
    /// Client IPs (exact or CIDR) allowed to call `/internal/kratos/*`.
    /// Empty = fail closed.
    pub webhook_allowed_clients: Vec<String>,
    /// Providers whose `email_verified` claim we trust for auto-linking.
    /// Anything not listed here can never auto-link.
    pub trusted_email_verified_providers: Vec<String>,
    /// Base URL for user-facing invitation links.
    #[serde(default = "default_app_base_url")]
    pub app_base_url: String,
}

fn default_app_base_url() -> String {
    "https://app.sanvi.dev".to_string()
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            session_cache_ttl_secs: 60,
            link_intent_ttl_secs: 600,
            invitation_ttl_days: 7,
            session_cookie_name: "ory_kratos_session".to_string(),
            webhook_secret: String::new(),
            webhook_allowed_clients: vec!["127.0.0.1".to_string(), "::1".to_string()],
            trusted_email_verified_providers: vec!["google".to_string(), "microsoft".to_string()],
            app_base_url: default_app_base_url(),
        }
    }
}

impl IdentityConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.session_cache_ttl_secs == 0 || self.session_cache_ttl_secs > 60 {
            anyhow::bail!("identity.session_cache_ttl_secs must be 1..=60");
        }
        if self.link_intent_ttl_secs == 0 || self.link_intent_ttl_secs > 3600 {
            anyhow::bail!("identity.link_intent_ttl_secs must be 1..=3600");
        }
        if self.invitation_ttl_days == 0 {
            anyhow::bail!("identity.invitation_ttl_days must be >= 1");
        }
        if self.session_cookie_name.is_empty() {
            anyhow::bail!("identity.session_cookie_name must not be empty");
        }
        if self.app_base_url.is_empty() {
            anyhow::bail!("identity.app_base_url must not be empty");
        }
        if !self.app_base_url.starts_with("http://") && !self.app_base_url.starts_with("https://") {
            anyhow::bail!("identity.app_base_url must be an http(s) URL");
        }
        Ok(())
    }

    /// Whether the provider's `email_verified` claim may auto-link.
    pub fn trusts_email_verified(&self, provider: &str) -> bool {
        self.trusted_email_verified_providers
            .iter()
            .any(|trusted| trusted.eq_ignore_ascii_case(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        IdentityConfig::default().validate().unwrap();
    }

    #[test]
    fn cache_ttl_is_bounded_by_the_plan() {
        let config = IdentityConfig {
            session_cache_ttl_secs: 120,
            ..IdentityConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn trust_list_is_exact() {
        let config = IdentityConfig::default();
        assert!(config.trusts_email_verified("google"));
        assert!(config.trusts_email_verified("Google"));
        assert!(!config.trusts_email_verified("random-provider"));
    }
}
