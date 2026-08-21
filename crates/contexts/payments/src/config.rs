//! Payments context configuration.
//!
//! Every secret is env-only (`SANVI__PAYMENTS__…`), never in config files.
//! The Connect restricted API key is a *different key* from the phase-04
//! platform key (`billing.stripe_api_key`), carrying only Connect-related
//! permissions — the composition root validates both distinctions at boot.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Application-fee configuration. Off by default in 0.10.0 (the platform
/// monetises via subscription); when a commercial model calls for it, the
/// fee rides `payment_intent_data.application_fee_amount` on a direct
/// charge — the only correct mechanism for this charge pattern.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ApplicationFeeConfig {
    /// Master switch. While false, no fee is ever sent on any
    /// tenant-transaction request.
    #[serde(default)]
    pub enabled: bool,
}

/// The i18n key an unmapped provider requirement code falls back to. The
/// catalog entry tells the tenant the raw code plus Stripe's requirements
/// help link — a new Stripe requirement degrades, never breaks (09.3).
pub const UNMAPPED_REQUIREMENT_KEY: &str = "payments.req.unmapped";

/// The requirement-code → i18n-key map shipped as data. Operators extend
/// or override it per environment from `[payments.requirement_keys]` in
/// config; codes added by the provider before this map learns them fall
/// back to [`UNMAPPED_REQUIREMENT_KEY`].
pub const DEFAULT_REQUIREMENT_KEYS: &[(&str, &str)] = &[
    (
        "individual.verification.document",
        "payments.req.id_document",
    ),
    (
        "company.verification.document",
        "payments.req.company_document",
    ),
    ("individual.id_number", "payments.req.id_number"),
    ("company.tax_id", "payments.req.tax_id"),
    ("business_profile.url", "payments.req.business_url"),
    (
        "business_profile.product_description",
        "payments.req.business_description",
    ),
    ("external_account", "payments.req.bank_account"),
    ("individual.address", "payments.req.address"),
    ("company.address", "payments.req.address"),
    ("individual.phone", "payments.req.phone"),
    ("individual.email", "payments.req.email"),
];

fn default_requirement_keys() -> HashMap<String, String> {
    DEFAULT_REQUIREMENT_KEYS
        .iter()
        .map(|(code, key)| ((*code).to_string(), (*key).to_string()))
        .collect()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PaymentsConfig {
    /// The entitlement key gating every payments route. Matches the
    /// access context's feature catalog seed (`payments.stripe_connect`).
    pub entitlement_key: String,
    /// Stripe API base URL for Connect calls (overridable for tests'
    /// mock servers; production points at `https://api.stripe.com`).
    pub connect_api_base: String,
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
    /// Provider requirement code → i18n message key (`payments.req.*`).
    /// Data, not code: new codes fall back to
    /// [`UNMAPPED_REQUIREMENT_KEY`] until this map (or the config file
    /// override) learns them.
    #[serde(default = "default_requirement_keys")]
    pub requirement_keys: HashMap<String, String>,
    /// Application-fee switch — off in 0.10.0.
    #[serde(default)]
    pub application_fee: ApplicationFeeConfig,
    /// Checkout-creation budget per bucket per minute (tenant, IP,
    /// customer reference each get their own bucket).
    #[serde(default = "default_checkout_rate_limit")]
    pub checkout_rate_limit_per_minute: u32,
    /// The `payments.checkout` release flag (09.4). Off → both checkout
    /// endpoints answer 503 `payments/provider-unavailable` while the
    /// payment webhook handlers stay deployed. On by default from this
    /// slice on; TASK-008 owns the staged-rollout cleanup.
    #[serde(default = "default_true")]
    pub checkout_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_checkout_rate_limit() -> u32 {
    30
}

impl Default for PaymentsConfig {
    fn default() -> Self {
        Self {
            entitlement_key: "payments.stripe_connect".to_string(),
            connect_api_base: "https://api.stripe.com".to_string(),
            connect_api_key: String::new(),
            stripe_api_version: "2026-06-24.dahlia".to_string(),
            connect_webhook_secret: String::new(),
            webhook_tolerance_secs: 300,
            webhook_allowed_clients: vec![],
            requirement_keys: default_requirement_keys(),
            application_fee: ApplicationFeeConfig::default(),
            checkout_rate_limit_per_minute: default_checkout_rate_limit(),
            checkout_enabled: true,
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
        if self.checkout_rate_limit_per_minute == 0 {
            anyhow::bail!("payments.checkout_rate_limit_per_minute must be >= 1");
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
        let config = PaymentsConfig {
            connect_api_key: "sk_live_oops".to_string(),
            ..PaymentsConfig::default()
        };
        assert!(config.validate().is_err());

        let config = PaymentsConfig {
            connect_webhook_secret: "not-a-secret".to_string(),
            ..PaymentsConfig::default()
        };
        assert!(config.validate().is_err());

        let config = PaymentsConfig {
            webhook_tolerance_secs: 0,
            ..PaymentsConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn requirement_keys_ship_as_data_with_a_documented_fallback() {
        let config = PaymentsConfig::default();
        // A known Stripe code maps to its message key…
        assert_eq!(
            config
                .requirement_keys
                .get("individual.verification.document")
                .map(String::as_str),
            Some("payments.req.id_document")
        );
        // …and the fallback key exists for everything the map has not
        // learned yet (new provider requirements degrade, never break).
        assert_ne!(UNMAPPED_REQUIREMENT_KEY, "payments.req.id_document");
        assert!(UNMAPPED_REQUIREMENT_KEY.starts_with("payments.req."));
    }
}
