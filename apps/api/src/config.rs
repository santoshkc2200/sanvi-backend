use serde::{Deserialize, Serialize};

use sanvi_http::ServerConfig;
use sanvi_messaging::RedisConfig;
use sanvi_persistence::DatabaseConfig;
use sanvi_telemetry::TelemetryConfig;

/// Ory endpoints. The containers run from phase 00; nothing consumes them
/// until phase 02 (identity) and phase 03 (access).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OryConfig {
    pub kratos_public_url: String,
    pub keto_read_url: String,
}

impl Default for OryConfig {
    fn default() -> Self {
        Self {
            kratos_public_url: "http://localhost:4433".to_string(),
            keto_read_url: "http://localhost:4466".to_string(),
        }
    }
}

/// Feature flags and phase gates (phase 01+ fill this in).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct FeaturesConfig {
    /// Never removed: lets later phases gate behaviour without config churn.
    #[serde(default)]
    pub placeholders: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Aggregate configuration for the API process.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AppConfig {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub telemetry: TelemetryConfig,
    pub ory: OryConfig,
    pub features: FeaturesConfig,
    /// Budget for each readiness probe.
    pub readiness_probe_secs: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            redis: RedisConfig::default(),
            telemetry: TelemetryConfig::default(),
            ory: OryConfig::default(),
            features: FeaturesConfig::default(),
            readiness_probe_secs: 2,
        }
    }
}

impl AppConfig {
    /// Fail fast with *all* configuration problems at once.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems: Vec<String> = Vec::new();
        if let Err(err) = self.server.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.database.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.redis.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.telemetry.validate() {
            problems.push(err.to_string());
        }
        if self.readiness_probe_secs == 0 {
            problems.push("readiness_probe_secs must be >= 1".to_string());
        }
        if !problems.is_empty() {
            anyhow::bail!("invalid configuration:\n  - {}", problems.join("\n  - "));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        AppConfig::default().validate().unwrap();
    }

    #[test]
    fn validation_reports_every_section() {
        let config = AppConfig {
            server: ServerConfig {
                host: String::new(),
                ..ServerConfig::default()
            },
            redis: RedisConfig {
                url: "not a url".to_string(),
            },
            readiness_probe_secs: 0,
            ..AppConfig::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("server.host"));
        assert!(err.contains("redis.url"));
        assert!(err.contains("readiness_probe_secs"));
    }
}
