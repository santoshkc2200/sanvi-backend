//! Configuration declarations for the accounting context.
//!
//! Values are loaded from environment overrides in deployed environments and
//! must never appear in API responses, diagnostics, or `Debug` output.

use sanvi_telemetry::Redacted;
use serde::{Deserialize, Serialize};

fn default_token_kek_id() -> String {
    "kek-1".into()
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountingConfig {
    pub freee_client_id: String,
    pub freee_client_secret: Redacted<String>,
    pub freee_redirect_uri: String,
    pub oauth_state_signing_secret: Redacted<String>,
    #[serde(default)]
    pub token_kek: Redacted<String>,
    #[serde(default = "default_token_kek_id")]
    pub token_kek_id: String,
}

impl Default for AccountingConfig {
    fn default() -> Self {
        Self {
            freee_client_id: String::new(),
            freee_client_secret: Redacted::default(),
            freee_redirect_uri: String::new(),
            oauth_state_signing_secret: Redacted::default(),
            token_kek: Redacted::default(),
            token_kek_id: default_token_kek_id(),
        }
    }
}

impl std::fmt::Debug for AccountingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountingConfig")
            .field("freee_client_id", &self.freee_client_id)
            .field("freee_client_secret", &"[REDACTED]")
            .field("freee_redirect_uri", &self.freee_redirect_uri)
            .field("oauth_state_signing_secret", &"[REDACTED]")
            .field("token_kek", &"[REDACTED]")
            .field("token_kek_id", &self.token_kek_id)
            .finish()
    }
}

impl AccountingConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn secrets_present(&self) -> bool {
        !self.freee_client_id.trim().is_empty()
            && !self.freee_client_secret.expose_secret().trim().is_empty()
            && !self
                .oauth_state_signing_secret
                .expose_secret()
                .trim()
                .is_empty()
            && !self.token_kek.expose_secret().trim().is_empty()
    }

    pub fn missing_secret_keys(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.freee_client_id.trim().is_empty() {
            missing.push("freee_client_id");
        }
        if self.freee_client_secret.expose_secret().trim().is_empty() {
            missing.push("freee_client_secret");
        }
        if self
            .oauth_state_signing_secret
            .expose_secret()
            .trim()
            .is_empty()
        {
            missing.push("oauth_state_signing_secret");
        }
        if self.token_kek.expose_secret().trim().is_empty() {
            missing.push("token_kek");
        }
        missing
    }

    pub fn token_kek_bytes(&self) -> anyhow::Result<[u8; 32]> {
        if self.token_kek.expose_secret().is_empty() {
            anyhow::bail!("accounting.token_kek is not configured");
        }
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            self.token_kek.expose_secret(),
        )
        .map_err(|err| anyhow::anyhow!("accounting.token_kek is not valid base64: {err}"))?;
        let kek: [u8; 32] = decoded.try_into().map_err(|_| {
            anyhow::anyhow!("accounting.token_kek must decode to exactly 32 bytes (AES-256)")
        })?;
        if self.token_kek_id.is_empty() {
            anyhow::bail!("accounting.token_kek_id must not be empty");
        }
        Ok(kek)
    }
}
