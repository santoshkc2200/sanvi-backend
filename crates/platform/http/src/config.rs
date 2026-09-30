use serde::{Deserialize, Serialize};

/// HTTP server settings. All values are validated together at startup.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// 0 = one worker per CPU core.
    pub workers: usize,
    pub shutdown_timeout_secs: u64,
    pub request_timeout_secs: u64,
    pub max_body_bytes: usize,
    pub max_concurrency: usize,
    pub cors: CorsConfig,
    pub rate_limit: RateLimitConfig,
    pub tenant: TenantPolicyConfig,
    #[serde(default)]
    pub security: SecurityHeadersConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            workers: 0,
            shutdown_timeout_secs: 30,
            request_timeout_secs: 30,
            max_body_bytes: 1024 * 1024,
            max_concurrency: 10_000,
            cors: CorsConfig::default(),
            rate_limit: RateLimitConfig::default(),
            tenant: TenantPolicyConfig::default(),
            security: SecurityHeadersConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Fail fast with *all* configuration problems at once.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems: Vec<String> = Vec::new();
        if self.host.trim().is_empty() {
            problems.push("server.host must not be empty".to_string());
        }
        if self.shutdown_timeout_secs == 0 {
            problems.push("server.shutdown_timeout_secs must be >= 1".to_string());
        }
        if self.request_timeout_secs == 0 {
            problems.push("server.request_timeout_secs must be >= 1".to_string());
        }
        if self.max_body_bytes == 0 {
            problems.push("server.max_body_bytes must be >= 1".to_string());
        }
        if self.max_concurrency == 0 {
            problems.push("server.max_concurrency must be >= 1".to_string());
        }
        if let Err(err) = self.cors.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.rate_limit.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.tenant.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.security.validate() {
            problems.push(err.to_string());
        }
        if !problems.is_empty() {
            anyhow::bail!(
                "invalid server configuration:\n  - {}",
                problems.join("\n  - ")
            );
        }
        Ok(())
    }
}

/// Tenant-resolution middleware policy.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TenantPolicyConfig {
    /// Path prefixes that never require a tenant (health, platform admin,
    /// public endpoints). Request path must start with one of these to be
    /// exempt from tenant resolution.
    pub public_prefixes: Vec<String>,
    /// Path prefixes a tenant may still reach while not `Active`
    /// (provisioning, suspended, or archived — billing/legal allow-list).
    /// Everything else gets 423 for any non-`Active` status.
    pub suspended_allowlist: Vec<String>,
    /// Per-tenant token bucket applied after resolution (optional).
    pub tenant_rate_limit: Option<RateLimitConfig>,
}

impl Default for TenantPolicyConfig {
    fn default() -> Self {
        Self {
            public_prefixes: vec![
                "/healthz".to_string(),
                "/readyz".to_string(),
                "/metrics".to_string(),
                "/openapi.json".to_string(),
                "/scalar".to_string(),
                "/api/v1/platform".to_string(),
                "/api/v1/public".to_string(),
                // FR-1102: the build probe is unauthenticated — RUM, the
                // error tracker and load tools stamp measurements with it.
                "/api/v1/system/build".to_string(),
            ],
            suspended_allowlist: vec!["/api/v1/billing".to_string()],
            tenant_rate_limit: None,
        }
    }
}

impl TenantPolicyConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self
            .public_prefixes
            .iter()
            .any(|prefix| !prefix.starts_with('/'))
        {
            anyhow::bail!("server.tenant.public_prefixes must start with '/'");
        }
        if let Some(limit) = &self.tenant_rate_limit {
            limit.validate()?;
        }
        Ok(())
    }

    pub fn is_public_path(&self, path: &str) -> bool {
        self.public_prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix))
    }

    pub fn is_suspended_allowlisted(&self, path: &str) -> bool {
        self.suspended_allowlist
            .iter()
            .any(|prefix| path.starts_with(prefix))
    }
}

/// Security response headers (NFR-1114): specified, not inherited from
/// framework defaults.
///
/// The five headers that never vary per environment (CSP, frame protection,
/// referrer policy, permissions policy, MIME sniffing protection) are pinned
/// as code constants in [`crate::middleware`]'s `SecurityHeaders` — making
/// them configurable invites weakening them per environment. HSTS is the one
/// explicit switch: it is meaningless without TLS, which local development
/// does not run, so it is absent (`None`) by default and enabled by the
/// staging and production overlays.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SecurityHeadersConfig {
    /// The `Strict-Transport-Security` directive. `None` emits no header.
    #[serde(default)]
    pub hsts: Option<HstsConfig>,
}

impl SecurityHeadersConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(hsts) = &self.hsts {
            hsts.validate()?;
        }
        Ok(())
    }
}

/// One HSTS directive, rendered verbatim as the header value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct HstsConfig {
    pub max_age_secs: u64,
    #[serde(default)]
    pub include_sub_domains: bool,
    /// Directs browsers to submit the host to the preload list. Opting in
    /// is an operations decision (removal from the preload list takes
    /// months), so no overlay sets this yet.
    #[serde(default)]
    pub preload: bool,
}

impl HstsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_age_secs == 0 {
            anyhow::bail!("server.security.hsts.max_age_secs must be >= 1");
        }
        // Preload-list submission requirements: a preload directive that
        // cannot qualify is a typo that silently opts into nothing.
        if self.preload {
            if !self.include_sub_domains {
                anyhow::bail!("server.security.hsts.preload requires include_sub_domains");
            }
            if self.max_age_secs < 31_536_000 {
                anyhow::bail!("server.security.hsts.preload requires max_age_secs >= 31536000");
            }
        }
        Ok(())
    }

    /// The `Strict-Transport-Security` value:
    /// `max-age=N[; includeSubDomains][; preload]`.
    pub fn header_value(&self) -> String {
        let mut value = format!("max-age={}", self.max_age_secs);
        if self.include_sub_domains {
            value.push_str("; includeSubDomains");
        }
        if self.preload {
            value.push_str("; preload");
        }
        value
    }
}

/// CORS policy: explicit origin allow-list, credentials, no wildcards.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CorsConfig {
    /// Explicit list of allowed origins. Empty = CORS disabled entirely.
    pub allowed_origins: Vec<String>,
    pub allow_credentials: bool,
    pub max_age_secs: u64,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec!["http://localhost:5173".to_string()],
            allow_credentials: true,
            max_age_secs: 600,
        }
    }
}

impl CorsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self
            .allowed_origins
            .iter()
            .any(|origin| origin == "*" || origin.contains('*'))
        {
            anyhow::bail!("cors.allowed_origins must not contain wildcards");
        }
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        !self.allowed_origins.is_empty()
    }
}

/// Per-IP rate limiting via a Redis token bucket.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RateLimitConfig {
    pub enabled: bool,
    /// Bucket capacity (burst allowance).
    pub burst: u64,
    /// Refill rate in tokens per second.
    pub per_second: u64,
    /// When Redis is unreachable: `true` lets requests through.
    pub fail_open: bool,
    /// Paths that never count against the limit.
    pub exempt_paths: Vec<String>,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            burst: 60,
            per_second: 10,
            fail_open: true,
            exempt_paths: vec![
                "/healthz".to_string(),
                "/readyz".to_string(),
                "/metrics".to_string(),
            ],
        }
    }
}

impl RateLimitConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.burst == 0 {
            anyhow::bail!("rate_limit.burst must be >= 1");
        }
        if self.per_second == 0 {
            anyhow::bail!("rate_limit.per_second must be >= 1");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        ServerConfig::default().validate().unwrap();
    }

    #[test]
    fn hsts_directive_renders_only_the_set_parts() {
        let base = HstsConfig {
            max_age_secs: 31_536_000,
            include_sub_domains: false,
            preload: false,
        };
        assert_eq!(base.header_value(), "max-age=31536000");
        assert_eq!(
            HstsConfig {
                include_sub_domains: true,
                ..base.clone()
            }
            .header_value(),
            "max-age=31536000; includeSubDomains"
        );
        assert_eq!(
            HstsConfig {
                include_sub_domains: true,
                preload: true,
                ..base
            }
            .header_value(),
            "max-age=31536000; includeSubDomains; preload"
        );
    }

    #[test]
    fn hsts_validation_rejects_impossible_directives() {
        let err = HstsConfig {
            max_age_secs: 0,
            include_sub_domains: false,
            preload: false,
        }
        .validate()
        .unwrap_err()
        .to_string();
        assert!(err.contains("max_age_secs"));

        // Preload that cannot qualify for the list is rejected at startup,
        // not silently ignored by every browser that reads it.
        let err = HstsConfig {
            max_age_secs: 31_536_000,
            include_sub_domains: false,
            preload: true,
        }
        .validate()
        .unwrap_err()
        .to_string();
        assert!(err.contains("include_sub_domains"));

        let err = HstsConfig {
            max_age_secs: 60,
            include_sub_domains: true,
            preload: true,
        }
        .validate()
        .unwrap_err()
        .to_string();
        assert!(err.contains("31536000"));
    }

    #[test]
    fn security_section_defaults_to_no_hsts_and_stays_valid() {
        assert_eq!(
            ServerConfig::default().security,
            SecurityHeadersConfig { hsts: None }
        );
        ServerConfig::default().validate().unwrap();
    }

    #[test]
    fn wildcard_origins_are_rejected() {
        let config = CorsConfig {
            allowed_origins: vec!["*".to_string()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validation_reports_all_problems_at_once() {
        let config = ServerConfig {
            host: String::new(),
            request_timeout_secs: 0,
            cors: CorsConfig {
                allowed_origins: vec!["*".to_string()],
                ..Default::default()
            },
            rate_limit: RateLimitConfig {
                burst: 0,
                ..Default::default()
            },
            ..ServerConfig::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("server.host"));
        assert!(err.contains("server.request_timeout_secs"));
        assert!(err.contains("wildcard"));
        assert!(err.contains("rate_limit.burst"));
    }
}
