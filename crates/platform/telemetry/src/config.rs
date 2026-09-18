use std::fmt;

use anyhow::Result;
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
///
/// The receiver is OpenObserve, which ingests OTLP directly on :5081 and
/// requires three pieces of gRPC metadata on every export: the organization,
/// the target stream, and a Basic credential.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OtlpConfig {
    pub enabled: bool,
    pub endpoint: String,
    /// OpenObserve organization; a fresh install has exactly one, `default`.
    pub organization: String,
    /// Stream the spans and metrics land in, sent as `stream-name`.
    pub stream: String,
    /// The base64 half of OpenObserve's `Authorization: Basic …` header — the
    /// value shown on its *Ingestion → OTEL Collector* page, or
    /// `printf 'email:password' | base64`. Set only via the environment
    /// (`SANVI__TELEMETRY__OTLP__AUTH_TOKEN`), never in committed files.
    /// Never appears in `Debug` or `Serialize` output — only
    /// `skip_serializing` here, so it still deserializes normally.
    #[serde(default, skip_serializing)]
    pub auth_token: String,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:5081".to_string(),
            organization: "default".to_string(),
            stream: "default".to_string(),
            auth_token: String::new(),
        }
    }
}

impl fmt::Debug for OtlpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtlpConfig")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint)
            .field("organization", &self.organization)
            .field("stream", &self.stream)
            .field("auth_token", &"<redacted>")
            .finish()
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
    /// Span sampling policy (TASK-020 step 5). Optional in config files so
    /// older configs keep loading; the default keeps every span (head
    /// ratio 1.0) with tail sampling off.
    #[serde(default)]
    pub sampling: SamplingConfig,
}

/// Sampling policy. Head sampling (`head_ratio`) decides at span start and
/// is always in effect — it is the fallback when the collector is down.
/// Tail sampling (`tail_sampling`, the task's `platform.tail_sampling`
/// flag) additionally keeps every error span and every span slower than
/// `latency_threshold_ms` even when the head coin dropped their trace.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SamplingConfig {
    /// Fraction of traces kept by the head sampler, 0.0–1.0.
    pub head_ratio: f64,
    /// Keep error spans and latency outliers regardless of the head coin.
    pub tail_sampling: bool,
    /// Spans at or above this duration are latency outliers.
    pub latency_threshold_ms: u64,
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            head_ratio: 1.0,
            tail_sampling: false,
            latency_threshold_ms: 1_000,
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "sanvi-api".to_string(),
            environment: "local".to_string(),
            log_format: TelemetryLogFormat::Text,
            default_level: "info".to_string(),
            otlp: OtlpConfig::default(),
            sampling: SamplingConfig::default(),
        }
    }
}

impl TelemetryConfig {
    pub fn otlp(&self) -> Result<&OtlpConfig> {
        if !self.otlp.enabled {
            return Ok(&self.otlp);
        }
        if self.otlp.endpoint.is_empty() {
            anyhow::bail!("telemetry.otlp.enabled is true but no endpoint is configured");
        }
        if self.otlp.organization.trim().is_empty() {
            anyhow::bail!("telemetry.otlp.organization must not be empty");
        }
        if self.otlp.stream.trim().is_empty() {
            anyhow::bail!("telemetry.otlp.stream must not be empty");
        }
        // OpenObserve rejects unauthenticated ingestion, and it does so per
        // export batch — long after startup, in a background task nobody is
        // watching. Refusing to boot is the louder failure.
        if self.otlp.auth_token.is_empty() {
            anyhow::bail!(
                "telemetry.otlp.enabled is true but no auth_token is configured \
                 (set SANVI__TELEMETRY__OTLP__AUTH_TOKEN)"
            );
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
        if !(0.0..=1.0).contains(&self.sampling.head_ratio) {
            anyhow::bail!(
                "telemetry.sampling.head_ratio must be within 0.0–1.0, got {}",
                self.sampling.head_ratio
            );
        }
        if self.sampling.tail_sampling && self.sampling.latency_threshold_ms == 0 {
            anyhow::bail!(
                "telemetry.sampling.latency_threshold_ms must be positive when tail_sampling is on"
            );
        }
        // No `.context("telemetry.otlp")` here: callers surface these with
        // `to_string()`, which prints only the outermost context and would
        // swallow the part that says what is actually wrong.
        self.otlp()?;
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

    /// An export target that OpenObserve would actually accept.
    fn enabled_otlp() -> OtlpConfig {
        OtlpConfig {
            enabled: true,
            auth_token: "cm9vdEBleGFtcGxlLmNvbTpwYXNzd29yZA==".to_string(),
            ..OtlpConfig::default()
        }
    }

    #[test]
    fn otlp_requires_endpoint_when_enabled() {
        let config = TelemetryConfig {
            otlp: OtlpConfig {
                endpoint: String::new(),
                ..enabled_otlp()
            },
            ..TelemetryConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn otlp_requires_auth_token_when_enabled() {
        let config = TelemetryConfig {
            otlp: OtlpConfig {
                auth_token: String::new(),
                ..enabled_otlp()
            },
            ..TelemetryConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn otlp_requires_organization_and_stream_when_enabled() {
        for otlp in [
            OtlpConfig {
                organization: "  ".to_string(),
                ..enabled_otlp()
            },
            OtlpConfig {
                stream: String::new(),
                ..enabled_otlp()
            },
        ] {
            let config = TelemetryConfig {
                otlp,
                ..TelemetryConfig::default()
            };
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn fully_configured_otlp_is_valid() {
        let config = TelemetryConfig {
            otlp: enabled_otlp(),
            ..TelemetryConfig::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn auth_token_never_leaves_the_process() {
        let config = TelemetryConfig {
            otlp: enabled_otlp(),
            ..TelemetryConfig::default()
        };
        let token = &config.otlp.auth_token;
        assert!(!format!("{config:?}").contains(token.as_str()));
        assert!(
            !serde_json::to_string(&config)
                .unwrap()
                .contains(token.as_str())
        );
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
