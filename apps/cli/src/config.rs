//! CLI configuration loading (database section only; shared helpers).

use anyhow::Context;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use sanvi_persistence::DatabaseConfig;
use serde::{Deserialize, Serialize};

/// The slice of configuration the CLI needs.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CliConfig {
    pub database: DatabaseConfig,
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
    Ok(config)
}

/// `config check`: load, validate, print (secrets redacted).
pub fn check(environment: &str) -> anyhow::Result<()> {
    let config = load(environment)?;
    println!("configuration OK (environment: {environment})");
    println!("{config:#?}");
    Ok(())
}
