//! Payments context configuration.
//!
//! Every secret is env-only (`SANVI__PAYMENTS__…`), never in config files.
//! The Connect restricted API key is a *different key* from the phase-04
//! platform key (`billing.stripe_api_key`), carrying only Connect-related
//! permissions — the composition root validates both distinctions at boot.

use serde::{Deserialize, Serialize};

/// Application-fee configuration. Off by default in 0.10.0 (the platform
/// monetises via subscription); when a commercial model calls for it, the
/// fee rides `payment_intent_data.application_fee_amount` on a direct
/// charge — the only correct mechanism for this charge pattern.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ApplicationFeeConfig {
    /// Master switch. While false, no fee is ever sent on any
    /// tenant-transaction request.
    #[serde(default)]
    pub enabled: bool,
}

impl Default for ApplicationFeeConfig {
    fn default() -> Self {
        Self { enabled: false }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PaymentsConfig {
    /// The entitlement key gating every payments route. Matches the
    /// access context's feature catalog seed (`payments.stripe_connect`).
    pub entitlement_key: String,
    /// Stripe restricted API key (`rk_`) for Connect operations —
    /// Connect permissions only, distinct from the phase-04 platform key.
    /// Empty disables provider calls (the catalog still answers). Secrets
    /// come from the environment, never config files.
    pub connect_api_key: String,
    /// Pinned Stripe API version for Connect calls; kept in lockstep with
    /// `billing.stripe_api_version` per environment (drift here means
    /// webhook payloads and API shapes disagree).
    pub stripe_api_version: String,
    /// Connect webhook signing secret (`whsec_`) for
    /// `/internal/webhooks/stripe/connect` — MUST differ from
    /// `billing.stripe_webhook_secret` (validated at boot). Empty rejects
    /// all Connect webhooks.
    pub connect_webhook_secret: String,
    /// How far a webhook timestamp may drift from now (seconds).
    pub webhook_tolerance_secs: i64,
    /// Client IPs allowed to POST Connect webhooks (defence in depth on
    /// top of signature verification). Empty = fail closed.
    pub webhook_allowed_clients: Vec<String>,
    /// Application-fee switch — off in 0.10.0.
    #[serde(default)]
    pub application_fee: ApplicationFeeConfig,
}

impl Default for PaymentsConfig {
    fn default() -> Self {
        Self {
            entitlement_key: "payments.stripe_connect".to_string(),
            connect_api_key: String::new(),
            stripe_api_version: "2026-06-24.dahlia".to_string(),
            connect_webhook_secret: String::new(),
            webhook_tolerance_secs: 300,
            webhook_allowed_clients: vec![],
            application_fee: ApplicationFeeConfig::default(),
        }
    }
}

impl PaymentsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.entitlement_key.is_empty() {
            anyhow::bail!("payments.entitlement_key must not be empty");
        }
        if !self.connect_api_key.is_empty() && !self.connect_api_key.starts_with("rk_") {
            // A Connect RAK only — sk_/whsec_ prefixes are configuration
            // mistakes the boot check should catch, not the operator's log.
            anyhow::bail!("payments.connect_api_key must be a Stripe restricted key (rk_)");
        }
        if !self.connect_webhook_secret.is_empty()
            && !self.connect_webhook_secret.starts_with("whsec_")
        {
            anyhow::bail!("payments.connect_webhook_secret must be a webhook secret (whsec_)");
        }
        if self.webhook_tolerance_secs <= 0 {
            anyhow::bail!("payments.webhook_tolerance_secs must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_fee_is_off() {
        let config = PaymentsConfig::default();
        config.validate().unwrap();
        assert!(!config.application_fee.enabled);
        assert_eq!(config.entitlement_key, "payments.stripe_connect");
    }

    #[test]
    fn validation_rejects_wrong_key_shapes() {
        let mut config = PaymentsConfig::default();
        config.connect_api_key = "sk_live_oops".to_string();
        assert!(config.validate().is_err());

        let mut config = PaymentsConfig::default();
        config.connect_webhook_secret = "not-a-secret".to_string();
        assert!(config.validate().is_err());

        let mut config = PaymentsConfig::default();
        config.webhook_tolerance_secs = 0;
        assert!(config.validate().is_err());
    }
}
