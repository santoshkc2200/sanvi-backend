use serde::{Deserialize, Serialize};

use sanvi_access::AccessConfig;
use sanvi_audit::AuditConfig;
use sanvi_billing::BillingConfig;
use sanvi_domains::DomainsConfig;
use sanvi_http::ServerConfig;
use sanvi_identity::IdentityConfig;
use sanvi_localization::LocalizationConfig;
use sanvi_messaging::RedisConfig;
use sanvi_payments::PaymentsConfig;
use sanvi_persistence::DatabaseConfig;
use sanvi_platform_admin::PlatformAdminConfig;
use sanvi_privacy::PrivacyConfig;
use sanvi_telemetry::TelemetryConfig;
use sanvi_tenancy::TenancyConfig;
use sanvi_theming::ThemingConfig;

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
///
/// Serde defaults are the *release* statement (a missing key means the
/// phase is GA): every shipped gate defaults on, including
/// `payments` since v0.10.0. The derived-Default path (all off) exists
/// only so [`AppConfig::default`] gives tests an inert app.
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
    /// Phase-07 gate: mounts the theming context's routes and state.
    #[serde(default = "default_true")]
    pub theming: bool,
    /// Phase-08 gate: mounts the domains context's routes and state, and
    /// installs the custom-domain hook into the tenancy resolver.
    #[serde(default = "default_true")]
    pub domains: bool,
    /// Phase-09 gate: mounts the payments context's routes and state.
    /// GA from v0.10.0 (09.7 flipped the default on). The rollback switch
    /// of last resort — per-surface control lives in the
    /// `payments.stripe_connect` entitlement and `payments.checkout`
    /// config, and webhook ingestion must never be disabled (in-flight
    /// payments must keep being recorded).
    #[serde(default = "default_true")]
    pub payments: bool,
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
    pub theming: ThemingConfig,
    pub domains: DomainsConfig,
    pub payments: PaymentsConfig,
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
            theming: ThemingConfig::default(),
            domains: DomainsConfig::default(),
            payments: PaymentsConfig::default(),
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
        if let Err(err) = self.theming.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.domains.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.payments.validate() {
            problems.push(err.to_string());
        }
        // Phase-09 invariant, enforced at boot: the Connect webhook
        // endpoint and the phase-04 platform endpoint must never share a
        // signing secret — a leaked one must not forge events for the
        // other (the cross-secret unit tests live in sanvi-payments).
        if !self.billing.stripe_webhook_secret.is_empty()
            && self.billing.stripe_webhook_secret == self.payments.connect_webhook_secret
        {
            problems.push(
                "billing.stripe_webhook_secret and payments.connect_webhook_secret must differ \
                 (the Connect and platform webhook endpoints are separate trust domains)"
                    .to_string(),
            );
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

    #[test]
    fn payments_ships_enabled_by_default() {
        // 09.7 flipped the phase flag for GA (v0.10.0): a config that
        // omits `features.payments` deserializes it to **on**. (The
        // derived-Default path stays all-off for tests; the release flip
        // lives in the serde defaults and configs/base.toml.)
        #[derive(Deserialize)]
        struct Minimal {
            features: FeaturesConfig,
        }
        let minimal: Minimal = serde_json::from_value(serde_json::json!({ "features": {} }))
            .expect("empty features object is a valid config");
        assert!(minimal.features.payments);
    }

    #[test]
    fn shared_webhook_secrets_are_rejected_at_boot() {
        let config = AppConfig {
            billing: sanvi_billing::BillingConfig {
                stripe_webhook_secret: "whsec_same".to_string(),
                ..sanvi_billing::BillingConfig::default()
            },
            payments: PaymentsConfig {
                connect_webhook_secret: "whsec_same".to_string(),
                ..PaymentsConfig::default()
            },
            ..AppConfig::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("must differ"));

        // Distinct secrets (and the unset/unset case) pass.
        let config = AppConfig {
            payments: PaymentsConfig {
                connect_webhook_secret: "whsec_connect".to_string(),
                ..PaymentsConfig::default()
            },
            ..AppConfig::default()
        };
        config.validate().unwrap();
    }
}
