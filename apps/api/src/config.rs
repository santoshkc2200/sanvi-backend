use serde::{Deserialize, Serialize};

use sanvi_access::AccessConfig;
use sanvi_audit::AuditConfig;
use sanvi_billing::BillingConfig;
use sanvi_http::ServerConfig;
use sanvi_identity::IdentityConfig;
use sanvi_localization::LocalizationConfig;
use sanvi_messaging::RedisConfig;
use sanvi_persistence::DatabaseConfig;
use sanvi_platform_admin::PlatformAdminConfig;
use sanvi_privacy::PrivacyConfig;
use sanvi_telemetry::TelemetryConfig;
use sanvi_tenancy::TenancyConfig;

/// Ory endpoints. Kratos public (flows/sessions), Kratos admin (identity
/// metadata), Keto read (checks) and write (tuple admin).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OryConfig {
    pub kratos_public_url: String,
    pub kratos_admin_url: String,
    pub keto_read_url: String,
    pub keto_write_url: String,
}

impl Default for OryConfig {
    fn default() -> Self {
        Self {
            kratos_public_url: "http://localhost:4433".to_string(),
            kratos_admin_url: "http://localhost:4434".to_string(),
            keto_read_url: "http://localhost:4466".to_string(),
            keto_write_url: "http://localhost:4467".to_string(),
        }
    }
}

/// Feature flags and phase gates (phase 01+ fill this in).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct FeaturesConfig {
    /// Phase-01 gate: mounts tenant resolution middleware and tenancy routes
    /// when enabled.
    #[serde(default = "default_true")]
    pub tenancy: bool,
    /// Phase-02 gate: mounts authentication middleware and the permission
    /// gate. When disabled, every route behaves public (documented
    /// phase-gate semantics).
    #[serde(default = "default_true")]
    pub identity: bool,
    /// Phase-03 gate: mounts the platform_admin/audit contexts, the feature
    /// gate and the impersonation middleware.
    #[serde(default = "default_true")]
    pub platform_admin: bool,
    /// Phase-04 gate: mounts the billing context's routes and state.
    #[serde(default = "default_true")]
    pub billing: bool,
    /// Phase-05 gate: mounts the privacy context's routes and state, and
    /// wires the real jurisdiction-driven privacy lock into the
    /// impersonation flow.
    #[serde(default = "default_true")]
    pub privacy: bool,
    /// Phase-06 gate: mounts locale negotiation, localized problem bodies
    /// and the localization context's routes.
    #[serde(default = "default_true")]
    pub localization: bool,
    /// Never removed: lets later phases gate behaviour without config churn.
    #[serde(default)]
    pub placeholders: std::collections::BTreeMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
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
    pub tenancy: TenancyConfig,
    pub identity: IdentityConfig,
    pub access: AccessConfig,
    pub platform_admin: PlatformAdminConfig,
    pub audit: AuditConfig,
    pub billing: BillingConfig,
    pub privacy: PrivacyConfig,
    pub localization: LocalizationConfig,
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
            tenancy: TenancyConfig::default(),
            identity: IdentityConfig::default(),
            access: AccessConfig::default(),
            platform_admin: PlatformAdminConfig::default(),
            audit: AuditConfig::default(),
            billing: BillingConfig::default(),
            privacy: PrivacyConfig::default(),
            localization: LocalizationConfig::default(),
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
        if let Err(err) = self.tenancy.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.identity.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.access.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.platform_admin.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.audit.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.billing.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.privacy.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.localization.validate() {
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
                ..RedisConfig::default()
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
