//! CLI configuration loading (database + ory sections; shared helpers).

use anyhow::Context;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use sanvi_kernel::IdsConfig;
use sanvi_persistence::DatabaseConfig;
use serde::{Deserialize, Serialize};

/// Ory endpoints (seed/access commands reach Keto).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CliOryConfig {
    pub keto_read_url: String,
    pub keto_write_url: String,
}

impl Default for CliOryConfig {
    fn default() -> Self {
        Self {
            keto_read_url: "http://localhost:4466".to_string(),
            keto_write_url: "http://localhost:4467".to_string(),
        }
    }
}

/// The slice of configuration the CLI needs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CliConfig {
    #[serde(default)]
    pub ids: IdsConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub ory: CliOryConfig,
    #[serde(default)]
    pub access: sanvi_access::AccessConfig,
    #[serde(default)]
    pub billing: sanvi_billing::BillingConfig,
    #[serde(default)]
    pub theming: sanvi_theming::ThemingConfig,
    #[serde(default)]
    pub domains: sanvi_domains::DomainsConfig,
    /// Phase 09: the payments section (the requirement-key map rides
    /// along; secrets stay env-only and are not needed by CLI commands).
    #[serde(default)]
    pub payments: sanvi_payments::PaymentsConfig,
}

/// Load `configs/base.toml` + `configs/{env}.toml` + `SANVI__` env overrides.
pub fn load(environment: &str) -> anyhow::Result<CliConfig> {
    let config: CliConfig = Figment::new()
        .merge(Toml::file("configs/base.toml"))
        .merge(Toml::file(format!("configs/{environment}.toml")))
        .merge(Env::prefixed("SANVI__").split("__"))
        .extract()
        .context("failed to load configuration")?;
    config.database.validate()?;
    config.access.validate()?;
    config.billing.validate()?;
    config.theming.validate()?;
    config.domains.validate()?;
    config.payments.validate()?;
    config.ids.resolve(environment, 3)?;
    Ok(config)
}

/// `config check`: load, validate, print (secrets redacted).
pub fn check(environment: &str) -> anyhow::Result<()> {
    let config = load(environment)?;
    println!("configuration OK (environment: {environment})");
    println!("{config:#?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use sanvi_kernel::IdsConfig;

    #[test]
    fn production_requires_an_explicit_node_id() {
        let err = IdsConfig::default().resolve("production", 3).unwrap_err();

        assert!(err.to_string().contains("production"));
    }

    #[test]
    fn local_cli_uses_reserved_node_three_when_unset() {
        let node = IdsConfig::default().resolve("local", 3).unwrap();

        assert_eq!(node.get(), 3);
    }
}
