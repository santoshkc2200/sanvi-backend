//! CLI configuration loading (database + ory sections; shared helpers).

use anyhow::Context;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
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
    pub database: DatabaseConfig,
    #[serde(default)]
    pub ory: CliOryConfig,
    #[serde(default)]
    pub billing: sanvi_billing::BillingConfig,
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
    config.billing.validate()?;
    Ok(config)
}

/// `config check`: load, validate, print (secrets redacted).
pub fn check(environment: &str) -> anyhow::Result<()> {
    let config = load(environment)?;
    println!("configuration OK (environment: {environment})");
    println!("{config:#?}");
    Ok(())
}
