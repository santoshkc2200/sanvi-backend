//! Configuration declarations for later platform adapters. Values are loaded
//! from environment overrides in deployed environments and must never appear
//! in API responses, diagnostics, or `Debug` output.

use sanvi_telemetry::Redacted;
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AdvertisingConfig {
    pub google_ads_developer_token: Redacted<String>,
    pub google_ads_client_id: String,
    pub google_ads_client_secret: Redacted<String>,
    pub meta_client_id: String,
    pub meta_client_secret: Redacted<String>,
    pub oauth_state_signing_secret: Redacted<String>,
    pub tracking_site_key_signing_secret: Redacted<String>,
    /// The KEK (key-encryption key) for the per-tenant OAuth-token envelope,
    /// base64-encoded 32 bytes (AES-256). Set only from a deployed secret
    /// (`SANVI__ADVERTISING__TOKEN_KEK`), never a config file. Empty
    /// disables connect/reconnect rather than silently storing tokens
    /// unencrypted.
    #[serde(default)]
    pub token_kek: Redacted<String>,
    /// KEK identifier recorded alongside wrapped DEKs (rotation adds a key,
    /// never overwrites one).
    #[serde(default = "default_token_kek_id")]
    pub token_kek_id: String,
    pub google_ads_api_version: String,
    pub meta_api_version: String,
    /// When `false` (default), every platform uses the fake in-process adapter
    /// (current, safe default — no credentials configured yet). When `true`,
    /// the composition root tries to build the real `GoogleAdsAdapter`/`MetaAdsAdapter`
    /// per platform.
    #[serde(default)]
    pub real_platform_adapters: bool,
    /// Explicit platform capability mode (`disabled`, `sandbox`, `live`).
    /// If not specified, falls back to `live` when `real_platform_adapters` is true, else `sandbox`.
    #[serde(default)]
    pub platform_mode: Option<String>,
    /// Explicit conversion sink capability mode (`disabled`, `sandbox`, `live`).
    #[serde(default)]
    pub conversion_sink_mode: Option<String>,
    /// Explicit audience sink capability mode (`disabled`, `sandbox`, `live`).
    #[serde(default)]
    pub audience_sink_mode: Option<String>,
    /// Platform-wide cap on in-flight adapter calls (the shared
    /// developer-token quota). See `adapters::rate_limit`.
    #[serde(default = "default_quota_platform_wide_permits")]
    pub quota_platform_wide_permits: usize,

    /// Per-connection cap nested inside the platform-wide one, so one
    /// tenant's bulk edit cannot claim every platform-wide slot.
    #[serde(default = "default_quota_per_connection_permits")]
    pub quota_per_connection_permits: usize,
    /// Hard cap on a `/ads/metrics*` query's `[from, to]` span — a bounded
    /// read is served from the rollup fast; an unbounded one is rejected
    /// outright rather than left to time out (TASK-016, slice 10.7).
    #[serde(default = "default_metrics_max_query_span_days")]
    pub metrics_max_query_span_days: i64,
    /// Days a rollup row stays marked `restating` — ad platforms keep
    /// revising conversions for roughly this long after the fact.
    #[serde(default = "default_metrics_restatement_days")]
    pub metrics_restatement_days: i64,
    /// Trailing window the ingestion sweep re-fetches on every tick, so a
    /// platform's later restatement of an already-ingested day is picked up
    /// without a separate backfill schedule.
    #[serde(default = "default_metrics_ingestion_backfill_days")]
    pub metrics_ingestion_backfill_days: i64,
    /// A connection whose last successful metrics ingestion is older than
    /// this is reported `stalled` by `/ads/metrics/freshness` rather than
    /// presented as current.
    #[serde(default = "default_metrics_freshness_stalled_after_hours")]
    pub metrics_freshness_stalled_after_hours: f64,
    /// FX rate provider for cross-currency cap math: `"none"` or `"ecb"`.
    /// `"none"` keeps `UnavailableFxRate` semantics — every cross-currency
    /// cap is refused until an operator configures a provider. `"ecb"`
    /// serves the ECB euro reference rates (free, no key), cached 24 h and
    /// degraded to unavailable on fetch failure.
    #[serde(default = "default_fx_provider")]
    pub fx_provider: String,
    /// ECB daily feed URL (override point for tests).
    #[serde(default = "default_fx_ecb_url")]
    pub fx_ecb_url: String,
    /// HTTP timeout for FX feed fetches.
    #[serde(default = "default_fx_timeout_secs")]
    pub fx_timeout_secs: u64,
}

const fn default_quota_platform_wide_permits() -> usize {
    20
}

const fn default_quota_per_connection_permits() -> usize {
    3
}

const fn default_metrics_max_query_span_days() -> i64 {
    92
}

const fn default_metrics_restatement_days() -> i64 {
    7
}

const fn default_metrics_ingestion_backfill_days() -> i64 {
    7
}

const fn default_metrics_freshness_stalled_after_hours() -> f64 {
    26.0
}

fn default_token_kek_id() -> String {
    "kek-1".to_string()
}

fn default_fx_provider() -> String {
    "ecb".to_string()
}

fn default_fx_ecb_url() -> String {
    "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-hist-90d.xml".to_string()
}

const fn default_fx_timeout_secs() -> u64 {
    5
}

impl std::fmt::Debug for AdvertisingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdvertisingConfig")
            .field("google_ads_developer_token", &"[REDACTED]")
            .field("google_ads_client_id", &"[REDACTED]")
            .field("google_ads_client_secret", &"[REDACTED]")
            .field("meta_client_id", &"[REDACTED]")
            .field("meta_client_secret", &"[REDACTED]")
            .field("oauth_state_signing_secret", &"[REDACTED]")
            .field("tracking_site_key_signing_secret", &"[REDACTED]")
            .field("token_kek", &"[REDACTED]")
            .field("token_kek_id", &self.token_kek_id)
            .field("google_ads_api_version", &self.google_ads_api_version)
            .field("meta_api_version", &self.meta_api_version)
            .field("real_platform_adapters", &self.real_platform_adapters)
            .field(
                "quota_platform_wide_permits",
                &self.quota_platform_wide_permits,
            )
            .field(
                "quota_per_connection_permits",
                &self.quota_per_connection_permits,
            )
            .field(
                "metrics_max_query_span_days",
                &self.metrics_max_query_span_days,
            )
            .field("metrics_restatement_days", &self.metrics_restatement_days)
            .field(
                "metrics_ingestion_backfill_days",
                &self.metrics_ingestion_backfill_days,
            )
            .field(
                "metrics_freshness_stalled_after_hours",
                &self.metrics_freshness_stalled_after_hours,
            )
            .field("fx_provider", &self.fx_provider)
            .field("fx_ecb_url", &self.fx_ecb_url)
            .field("fx_timeout_secs", &self.fx_timeout_secs)
            .finish()
    }
}

impl Default for AdvertisingConfig {
    fn default() -> Self {
        Self {
            google_ads_developer_token: Redacted::default(),
            google_ads_client_id: String::new(),
            google_ads_client_secret: Redacted::default(),
            meta_client_id: String::new(),
            meta_client_secret: Redacted::default(),
            oauth_state_signing_secret: Redacted::default(),
            tracking_site_key_signing_secret: Redacted::default(),
            token_kek: Redacted::default(),
            token_kek_id: default_token_kek_id(),
            google_ads_api_version: "v22".to_string(),
            meta_api_version: "v24.0".to_string(),
            real_platform_adapters: false,
            platform_mode: None,
            conversion_sink_mode: None,
            audience_sink_mode: None,
            quota_platform_wide_permits: default_quota_platform_wide_permits(),
            quota_per_connection_permits: default_quota_per_connection_permits(),
            metrics_max_query_span_days: default_metrics_max_query_span_days(),
            metrics_restatement_days: default_metrics_restatement_days(),
            metrics_ingestion_backfill_days: default_metrics_ingestion_backfill_days(),
            metrics_freshness_stalled_after_hours: default_metrics_freshness_stalled_after_hours(),
            fx_provider: default_fx_provider(),
            fx_ecb_url: default_fx_ecb_url(),
            fx_timeout_secs: default_fx_timeout_secs(),
        }
    }
}

impl AdvertisingConfig {
    /// Resolve one optional mode string, falling back to the mode implied by
    /// `real_platform_adapters`. A *malformed* value is an error, never a
    /// silent fallback: `platform_mode = "liv"` degrading to sandbox is the
    /// same class of defect as the fake-adapter fallback this replaced.
    fn resolve_mode(
        raw: Option<&str>,
        field: &str,
        real_adapters: bool,
    ) -> anyhow::Result<sanvi_kernel::CapabilityMode> {
        match raw {
            Some(value) => sanvi_kernel::CapabilityMode::parse(value)
                .map_err(|err| anyhow::anyhow!("advertising.{field}: {err}")),
            None if real_adapters => Ok(sanvi_kernel::CapabilityMode::Live),
            None => Ok(sanvi_kernel::CapabilityMode::Sandbox),
        }
    }

    pub fn resolved_platform_mode(&self) -> anyhow::Result<sanvi_kernel::CapabilityMode> {
        Self::resolve_mode(
            self.platform_mode.as_deref(),
            "platform_mode",
            self.real_platform_adapters,
        )
    }

    pub fn resolved_conversion_sink_mode(&self) -> anyhow::Result<sanvi_kernel::CapabilityMode> {
        Self::resolve_mode(
            self.conversion_sink_mode.as_deref(),
            "conversion_sink_mode",
            self.real_platform_adapters,
        )
    }

    pub fn resolved_audience_sink_mode(&self) -> anyhow::Result<sanvi_kernel::CapabilityMode> {
        Self::resolve_mode(
            self.audience_sink_mode.as_deref(),
            "audience_sink_mode",
            self.real_platform_adapters,
        )
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.google_ads_api_version.trim().is_empty() || self.meta_api_version.trim().is_empty()
        {
            anyhow::bail!("advertising API versions must not be empty");
        }
        if self.quota_platform_wide_permits == 0 || self.quota_per_connection_permits == 0 {
            anyhow::bail!("advertising quota permits must be at least 1");
        }
        if self.quota_per_connection_permits > self.quota_platform_wide_permits {
            anyhow::bail!(
                "advertising per-connection quota permits cannot exceed the platform-wide budget"
            );
        }
        if self.metrics_max_query_span_days <= 0 {
            anyhow::bail!("advertising metrics_max_query_span_days must be positive");
        }
        if self.metrics_restatement_days < 0 {
            anyhow::bail!("advertising metrics_restatement_days must not be negative");
        }
        if self.metrics_ingestion_backfill_days <= 0 {
            anyhow::bail!("advertising metrics_ingestion_backfill_days must be positive");
        }
        if self.metrics_freshness_stalled_after_hours <= 0.0 {
            anyhow::bail!("advertising metrics_freshness_stalled_after_hours must be positive");
        }
        match self.fx_provider.trim().to_ascii_lowercase().as_str() {
            "none" | "ecb" => {}
            other => {
                anyhow::bail!("advertising fx_provider must be \"none\" or \"ecb\", got {other:?}");
            }
        }
        if self.fx_provider.trim().eq_ignore_ascii_case("ecb") {
            if self.fx_ecb_url.trim().is_empty() {
                anyhow::bail!("advertising fx_ecb_url must not be empty when fx_provider is ecb");
            }
            if self.fx_timeout_secs == 0 {
                anyhow::bail!("advertising fx_timeout_secs must be positive");
            }
        }
        Ok(())
    }

    pub fn validate_oauth_enabled(&self) -> anyhow::Result<()> {
        if self.oauth_state_signing_secret.len() < 32 {
            anyhow::bail!("advertising OAuth state signing secret must be at least 32 bytes");
        }
        Ok(())
    }

    /// The site key that authenticates `/public/track` is an HMAC over
    /// `(tenant_id, origin)`. HMAC accepts a key of any length, so the
    /// empty default produces signatures anyone can recompute for any
    /// tenant — refuse to start rather than serve a forgeable beacon.
    pub fn validate_tracking_enabled(&self) -> anyhow::Result<()> {
        if self.tracking_site_key_signing_secret.len()
            < crate::application::tracking::site_key::MIN_SIGNING_SECRET_BYTES
        {
            anyhow::bail!(
                "advertising tracking site key signing secret must be at least {} bytes",
                crate::application::tracking::site_key::MIN_SIGNING_SECRET_BYTES
            );
        }
        Ok(())
    }

    /// `true` once a KEK is configured — connect/reconnect refuse to run
    /// without one rather than storing a refresh token unencrypted.
    pub fn token_encryption_enabled(&self) -> bool {
        !self.token_kek.is_empty()
    }

    /// Decodes and validates `token_kek`. Call at startup and again before
    /// wiring the crypto adapter — never trust an unvalidated string into
    /// `SqlxTenantEnvelopeCrypto::new`.
    pub fn token_kek_bytes(&self) -> anyhow::Result<[u8; 32]> {
        if self.token_kek.is_empty() {
            anyhow::bail!("advertising.token_kek is not configured");
        }
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            self.token_kek.expose_secret(),
        )
        .map_err(|err| anyhow::anyhow!("advertising.token_kek is not valid base64: {err}"))?;
        let kek: [u8; 32] = decoded.try_into().map_err(|_| {
            anyhow::anyhow!("advertising.token_kek must decode to exactly 32 bytes (AES-256)")
        })?;
        if self.token_kek_id.is_empty() {
            anyhow::bail!("advertising.token_kek_id must not be empty");
        }
        Ok(kek)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(bytes: &[u8]) -> String {
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
    }

    #[test]
    fn tracking_refuses_to_start_without_a_real_site_key_secret() {
        let config = AdvertisingConfig::default();
        assert!(config.tracking_site_key_signing_secret.is_empty());
        assert!(config.validate_tracking_enabled().is_err());

        let configured = AdvertisingConfig {
            tracking_site_key_signing_secret: Redacted::new("a".repeat(32)),
            ..AdvertisingConfig::default()
        };
        assert!(configured.validate_tracking_enabled().is_ok());
    }

    #[test]
    fn token_encryption_is_disabled_until_a_kek_is_configured() {
        let config = AdvertisingConfig::default();
        assert!(!config.token_encryption_enabled());
        assert!(config.token_kek_bytes().is_err());
    }

    #[test]
    fn missing_kek_fields_deserialize_to_safe_defaults() {
        let config: AdvertisingConfig = serde_json::from_value(serde_json::json!({
            "google_ads_developer_token": "",
            "google_ads_client_id": "",
            "google_ads_client_secret": "",
            "meta_client_id": "",
            "meta_client_secret": "",
            "oauth_state_signing_secret": "",
            "tracking_site_key_signing_secret": "",
            "google_ads_api_version": "v22",
            "meta_api_version": "v24.0"
        }))
        .unwrap();

        assert_eq!(config.token_kek.expose_secret(), "");
        assert_eq!(config.token_kek_id, "kek-1");
        assert!(!config.real_platform_adapters);
        assert_eq!(config.fx_provider, "ecb");
        assert!(!config.fx_ecb_url.is_empty());
        assert!(config.fx_timeout_secs > 0);
    }

    #[test]
    fn real_platform_adapters_defaults_to_false() {
        let config = AdvertisingConfig::default();
        assert!(!config.real_platform_adapters);
    }

    #[test]
    fn a_32_byte_kek_is_accepted() {
        let config = AdvertisingConfig {
            token_kek: Redacted::new(encode(&[5u8; 32])),
            ..AdvertisingConfig::default()
        };
        assert!(config.token_encryption_enabled());
        assert_eq!(config.token_kek_bytes().unwrap(), [5u8; 32]);
    }

    #[test]
    fn rejects_short_or_garbled_keks() {
        let too_short = AdvertisingConfig {
            token_kek: Redacted::new(encode(&[1u8; 16])),
            ..AdvertisingConfig::default()
        };
        assert!(too_short.token_kek_bytes().is_err());

        let garbled = AdvertisingConfig {
            token_kek: Redacted::new("not base64 !!".to_string()),
            ..AdvertisingConfig::default()
        };
        assert!(garbled.token_kek_bytes().is_err());
    }

    #[test]
    fn empty_kek_id_is_rejected_even_with_a_valid_kek() {
        let config = AdvertisingConfig {
            token_kek: Redacted::new(encode(&[5u8; 32])),
            token_kek_id: String::new(),
            ..AdvertisingConfig::default()
        };
        assert!(config.token_kek_bytes().is_err());
    }
}
