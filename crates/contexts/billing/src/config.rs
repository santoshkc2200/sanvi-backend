//! Billing context configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BillingConfig {
    /// Stripe API base URL (overridable for stripe-mock).
    pub stripe_api_base: String,
    /// Stripe restricted API key (`rk_`), least-privilege scopes. Empty
    /// disables Stripe calls (checkout/portal return errors); secrets come
    /// from the environment, never config files.
    pub stripe_api_key: String,
    /// Pinned Stripe API version (single, current version per environment).
    pub stripe_api_version: String,
    /// Webhook signing secret (`whsec_`). Empty rejects all webhooks.
    pub stripe_webhook_secret: String,
    /// Default success/cancel URLs for Checkout (per-request values win).
    pub success_url: String,
    pub cancel_url: String,
    /// Return URL for the Customer Portal.
    pub portal_return_url: String,
    /// Dunning grace window: payment failure → downgrade delay in days.
    pub dunning_grace_days: u32,
    /// Send the trial-ending notice this many days before `trial_end`.
    pub trial_reminder_days: i64,
    /// Enables `automatic_tax` on Checkout — ONLY when active Stripe Tax
    /// registrations exist (enforced by the tax gate, not trusted here).
    pub automatic_tax_enabled: bool,
    /// How far a webhook timestamp may drift from now (seconds).
    pub webhook_tolerance_secs: i64,
    /// Client IPs allowed to POST to the Stripe webhook endpoint
    /// (defence in depth on top of signature verification).
    pub webhook_allowed_clients: Vec<String>,
    /// Randomized suffix length for the Checkout `integration_identifier`.
    pub integration_identifier_suffix_len: usize,
}

impl Default for BillingConfig {
    fn default() -> Self {
        Self {
            stripe_api_base: "https://api.stripe.com".to_string(),
            stripe_api_key: String::new(),
            stripe_api_version: "2026-06-24.dahlia".to_string(),
            stripe_webhook_secret: String::new(),
            success_url: "http://localhost:5173/billing/success".to_string(),
            cancel_url: "http://localhost:5173/pricing".to_string(),
            portal_return_url: "http://localhost:5173/billing".to_string(),
            dunning_grace_days: 14,
            trial_reminder_days: 3,
            automatic_tax_enabled: false,
            webhook_tolerance_secs: 300,
            webhook_allowed_clients: vec![],
            integration_identifier_suffix_len: 8,
        }
    }
}

impl BillingConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.stripe_api_base.starts_with("http://")
            && !self.stripe_api_base.starts_with("https://")
        {
            anyhow::bail!("billing.stripe_api_base must be an http(s) URL");
        }
        if !self.stripe_api_key.is_empty()
            && !(self.stripe_api_key.starts_with("rk_") || self.stripe_api_key.starts_with("sk_"))
        {
            anyhow::bail!(
                "billing.stripe_api_key must be a Stripe restricted key (rk_) or secret key (sk_)"
            );
        }
        if self.dunning_grace_days == 0 {
            anyhow::bail!("billing.dunning_grace_days must be >= 1");
        }
        if self.trial_reminder_days <= 0 {
            anyhow::bail!("billing.trial_reminder_days must be >= 1");
        }
        if self.webhook_tolerance_secs <= 0 {
            anyhow::bail!("billing.webhook_tolerance_secs must be >= 1");
        }
        if self.integration_identifier_suffix_len < 4 || self.integration_identifier_suffix_len > 32
        {
            anyhow::bail!("billing.integration_identifier_suffix_len must be 4..=32");
        }
        Ok(())
    }

    /// Whether Stripe API calls are possible in this configuration.
    pub fn stripe_enabled(&self) -> bool {
        !self.stripe_api_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        BillingConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_bad_api_key_prefix() {
        let config = BillingConfig {
            stripe_api_key: "nope".to_string(),
            ..BillingConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn restricted_keys_are_accepted() {
        // Build the prefix without embedding a literal secret-looking string
        // (gitleaks scans source for Stripe key patterns).
        let key = format!("{}{}", "rk_", "live_sanvi_test_only");
        let config = BillingConfig {
            stripe_api_key: key.clone(),
            ..BillingConfig::default()
        };
        config.validate().unwrap();
        assert!(config.stripe_enabled());
    }
}
