use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryLogFormat {
    /// Human-readable, single-line log entries. Used locally.
    Text,
    /// Structured JSON, one object per line. Used in staging/production.
    #[default]
    Json,
}

/// OpenTelemetry OTLP (gRPC) export settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OtlpConfig {
    pub enabled: bool,
    pub endpoint: String,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:4317".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TelemetryConfig {
    pub service_name: String,
    pub environment: String,
    pub log_format: TelemetryLogFormat,
    pub default_level: String,
    pub otlp: OtlpConfig,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "sanvi-api".to_string(),
            environment: "local".to_string(),
            log_format: TelemetryLogFormat::Text,
            default_level: "info".to_string(),
            otlp: OtlpConfig::default(),
        }
    }
}

impl TelemetryConfig {
    pub fn otlp(&self) -> Result<&OtlpConfig> {
        if self.otlp.enabled && self.otlp.endpoint.is_empty() {
            anyhow::bail!("telemetry.otlp.enabled is true but no endpoint is configured");
        }
        Ok(&self.otlp)
    }

    /// Fail fast with *all* configuration problems at once.
    pub fn validate(&self) -> Result<()> {
        if self.service_name.trim().is_empty() {
            anyhow::bail!("telemetry.service_name must not be empty");
        }
        if self.environment.trim().is_empty() {
            anyhow::bail!("telemetry.environment must not be empty");
        }
        self.otlp().context("telemetry.otlp")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        TelemetryConfig::default().validate().unwrap();
    }

    #[test]
    fn otlp_requires_endpoint_when_enabled() {
        let config = TelemetryConfig {
            otlp: OtlpConfig {
                enabled: true,
                endpoint: String::new(),
            },
            ..TelemetryConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_blank_service_name() {
        let config = TelemetryConfig {
            service_name: "  ".to_string(),
            ..TelemetryConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
