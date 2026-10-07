use serde::{Deserialize, Serialize};

use crate::abuse::AbuseSurface;
use crate::compression::CompressionConfig;

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
    /// Idle keep-alive timeout per connection. `0` disables keep-alive.
    #[serde(default = "default_keep_alive_secs")]
    pub keep_alive_secs: u64,
    pub cors: CorsConfig,
    pub rate_limit: RateLimitConfig,
    pub tenant: TenantPolicyConfig,
    #[serde(default)]
    pub security: SecurityHeadersConfig,
    #[serde(default)]
    pub abuse: AbuseControlsConfig,
    /// Response compression with a size floor (TASK-021 step 7).
    #[serde(default)]
    pub compression: CompressionConfig,
    /// Server-side SSR response cache for the anonymous storefront reads
    /// (TASK-022; the `platform.ssr_cache` flag). Off — the default — is the
    /// correct-but-slower behaviour: every public read renders.
    #[serde(default)]
    pub ssr_cache: SsrCacheConfig,
}

fn default_keep_alive_secs() -> u64 {
    // actix's own default is 5 s (client churn on every idle gap); 60 s sits
    // at the ubiquitous load-balancer idle default, so the server almost
    // never holds a connection the balancer has already dropped. Recorded
    // in `benchmarks/transport-2026-10-06.md`.
    60
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
            keep_alive_secs: default_keep_alive_secs(),
            cors: CorsConfig::default(),
            rate_limit: RateLimitConfig::default(),
            tenant: TenantPolicyConfig::default(),
            security: SecurityHeadersConfig::default(),
            abuse: AbuseControlsConfig::default(),
            compression: CompressionConfig::default(),
            ssr_cache: SsrCacheConfig::default(),
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
        if let Err(err) = self.abuse.validate() {
            problems.push(err.to_string());
        }
        if let Err(err) = self.ssr_cache.validate() {
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

/// Server-side SSR response cache (FR-1111, TASK-022; the
/// `platform.ssr_cache` flag).
///
/// Caches the anonymous storefront read responses (the `/api/v1/public`
/// resources across theming, localization, billing and privacy) keyed by
/// `(tenant, locale, theme revision, device class)`, with single-flight on
/// miss. Authenticated responses — anything arriving with an `Authorization`
/// header — and responses carrying `Set-Cookie` are never served from or
/// written to the cache, asserted by tests rather than comments.
///
/// Invalidation is event-driven: a theme publish or content change drops the
/// affected entries within ~2 s (the outbox poll interval is 1 s plus stream
/// delivery). The TTL is only a backstop for entries whose invalidation
/// event was missed, never the primary expiry mechanism. Absent Redis (or
/// any cache error) degrades to an always-miss, which is the flag-off
/// behaviour.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SsrCacheConfig {
    /// The `platform.ssr_cache` flag. `false` (the default) is the
    /// pre-cache behaviour: every public read renders.
    pub enabled: bool,
    /// Backstop TTL for a cached storefront entry, in seconds. Invalidation
    /// on publish/content change is event-driven and does not wait for it.
    pub ttl_secs: u64,
}

impl Default for SsrCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_secs: 300,
        }
    }
}

impl SsrCacheConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.enabled && self.ttl_secs == 0 {
            anyhow::bail!("server.ssr_cache.ttl_secs must be >= 1 when ssr_cache is enabled");
        }
        Ok(())
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

/// Abuse quotas (TASK-024, FR-1117). The shipping posture is shadow mode:
/// counting is live on every surface with `would_block` metrics recorded,
/// and only the public tracking beacon enforces — it already answers an
/// indistinguishable empty `202` to everything, so enforcing there changes
/// no observable contract. Tuning thresholds against real traffic is a
/// configuration change, not a code change (`docs/release/needs-humans.md`).
///
/// Serde shape (every section optional; absent means [`Default`], which is
/// this posture):
///
/// ```toml
/// [server.abuse.tracking]
/// enforce = true
/// per_ip = { limit = 120, window_secs = 60 }
/// per_origin = { limit = 600, window_secs = 60 }
/// ```
///
/// A section that is present overrides that surface; a section that is
/// absent keeps the shipping posture — naming one surface must not
/// silently turn counting off on the others, and `SurfaceAbuseConfig`'s
/// empty default fails validation ("no quota dimension"), so counting
/// cannot be disabled by omission. To effectively retire a quota, raise
/// its limit and keep counting.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct AbuseControlsConfig {
    #[serde(default = "default_tracking_surface")]
    pub tracking: SurfaceAbuseConfig,
    #[serde(default = "default_invitation_surface")]
    pub invitation: SurfaceAbuseConfig,
    #[serde(default = "default_domain_purchase_surface")]
    pub domain_purchase: SurfaceAbuseConfig,
    #[serde(default = "default_signup_surface")]
    pub signup: SurfaceAbuseConfig,
}

fn default_tracking_surface() -> SurfaceAbuseConfig {
    SurfaceAbuseConfig {
        enforce: true,
        per_ip: Some(WindowQuota {
            limit: 120,
            window_secs: 60,
        }),
        per_origin: Some(WindowQuota {
            limit: 600,
            window_secs: 60,
        }),
        per_tenant: None,
    }
}

fn default_invitation_surface() -> SurfaceAbuseConfig {
    SurfaceAbuseConfig {
        enforce: false,
        per_ip: Some(WindowQuota {
            limit: 30,
            window_secs: 3600,
        }),
        per_origin: None,
        per_tenant: Some(WindowQuota {
            limit: 60,
            window_secs: 3600,
        }),
    }
}

fn default_domain_purchase_surface() -> SurfaceAbuseConfig {
    SurfaceAbuseConfig {
        enforce: false,
        per_ip: Some(WindowQuota {
            limit: 20,
            window_secs: 3600,
        }),
        per_origin: None,
        per_tenant: Some(WindowQuota {
            limit: 30,
            window_secs: 3600,
        }),
    }
}

fn default_signup_surface() -> SurfaceAbuseConfig {
    SurfaceAbuseConfig {
        enforce: false,
        per_ip: Some(WindowQuota {
            limit: 60,
            window_secs: 3600,
        }),
        per_origin: None,
        per_tenant: None,
    }
}

/// The shipped posture rather than an all-zero derive: counting on for
/// every FR-1117 surface the backend owns, enforcement on for tracking
/// only. Password reset has no backend surface to count (it is a Kratos
/// recovery flow); signup counts the internal registration webhook.
impl Default for AbuseControlsConfig {
    fn default() -> Self {
        Self {
            tracking: default_tracking_surface(),
            invitation: default_invitation_surface(),
            domain_purchase: default_domain_purchase_surface(),
            signup: default_signup_surface(),
        }
    }
}

impl AbuseControlsConfig {
    pub fn for_surface(&self, surface: AbuseSurface) -> &SurfaceAbuseConfig {
        match surface {
            AbuseSurface::Tracking => &self.tracking,
            AbuseSurface::Invitation => &self.invitation,
            AbuseSurface::DomainPurchase => &self.domain_purchase,
            AbuseSurface::Signup => &self.signup,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems: Vec<String> = Vec::new();
        for (name, surface) in [
            ("tracking", &self.tracking),
            ("invitation", &self.invitation),
            ("domain_purchase", &self.domain_purchase),
            ("signup", &self.signup),
        ] {
            if surface.per_ip.is_none()
                && surface.per_origin.is_none()
                && surface.per_tenant.is_none()
            {
                problems.push(format!("server.abuse.{name} has no quota dimension"));
                continue;
            }
            for (dimension, quota) in [
                ("per_ip", surface.per_ip),
                ("per_origin", surface.per_origin),
                ("per_tenant", surface.per_tenant),
            ] {
                if let Some(quota) = quota {
                    problems.extend(quota.problems(&format!("server.abuse.{name}.{dimension}")));
                }
            }
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

/// One surface's controls. `enforce = false` is shadow mode: counting and
/// `would_block` metrics only, the response is never touched.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SurfaceAbuseConfig {
    #[serde(default)]
    pub enforce: bool,
    #[serde(default)]
    pub per_ip: Option<WindowQuota>,
    #[serde(default)]
    pub per_origin: Option<WindowQuota>,
    #[serde(default)]
    pub per_tenant: Option<WindowQuota>,
}

/// One fixed window: at most `limit` events per `window_secs` per key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WindowQuota {
    pub limit: u64,
    pub window_secs: u64,
}

impl WindowQuota {
    fn problems(&self, prefix: &str) -> Vec<String> {
        let mut problems = Vec::new();
        if self.limit == 0 {
            problems.push(format!("{prefix}.limit must be >= 1"));
        }
        if self.window_secs == 0 {
            problems.push(format!("{prefix}.window_secs must be >= 1"));
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        ServerConfig::default().validate().unwrap();
    }

    /// The `platform.ssr_cache` flag ships off (TASK-022 11.3a): a config
    /// that does not mention the section at all is the pre-cache behaviour,
    /// and an enabled cache cannot have a zero backstop TTL.
    #[test]
    fn ssr_cache_absent_means_off_and_enabled_requires_a_backstop_ttl() {
        let config: ServerConfig = toml::from_str(
            r#"
host = "0.0.0.0"
port = 8080
workers = 0
shutdown_timeout_secs = 30
request_timeout_secs = 30
max_body_bytes = 1048576
max_concurrency = 10000
[cors]
allowed_origins = []
allow_credentials = true
max_age_secs = 600
[rate_limit]
enabled = false
burst = 60
per_second = 10
fail_open = true
exempt_paths = []
[tenant]
public_prefixes = []
suspended_allowlist = []
"#,
        )
        .unwrap();
        assert!(!config.ssr_cache.enabled, "absent section = flag off");
        assert_eq!(config.ssr_cache.ttl_secs, 300);

        let enabled_no_ttl: SsrCacheConfig =
            toml::from_str("enabled = true\nttl_secs = 0").unwrap();
        assert!(enabled_no_ttl.validate().is_err());
        let enabled = SsrCacheConfig {
            enabled: true,
            ttl_secs: 300,
        };
        enabled.validate().unwrap();
    }

    #[test]
    fn abuse_defaults_are_shadow_everywhere_except_tracking() {
        let config = AbuseControlsConfig::default();
        assert!(config.tracking.enforce, "tracking enforces from the start");
        assert!(!config.invitation.enforce);
        assert!(!config.domain_purchase.enforce);
        assert!(!config.signup.enforce);
        // Counting is live on every surface the backend owns.
        assert!(config.tracking.per_ip.is_some());
        assert!(config.invitation.per_ip.is_some());
        assert!(config.domain_purchase.per_ip.is_some());
        assert!(config.signup.per_ip.is_some());
        config.validate().unwrap();
    }

    #[test]
    fn abuse_validation_names_the_offending_key() {
        let config = AbuseControlsConfig {
            tracking: SurfaceAbuseConfig {
                enforce: true,
                per_ip: Some(WindowQuota {
                    limit: 0,
                    window_secs: 60,
                }),
                per_origin: None,
                per_tenant: None,
            },
            ..Default::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("server.abuse.tracking.per_ip.limit"), "{err}");

        let empty = AbuseControlsConfig {
            invitation: SurfaceAbuseConfig::default(),
            ..Default::default()
        };
        let err = empty.validate().unwrap_err().to_string();
        assert!(err.contains("server.abuse.invitation"), "{err}");

        let zero_window = AbuseControlsConfig {
            signup: SurfaceAbuseConfig {
                enforce: false,
                per_ip: Some(WindowQuota {
                    limit: 10,
                    window_secs: 0,
                }),
                per_origin: None,
                per_tenant: None,
            },
            ..Default::default()
        };
        let err = zero_window.validate().unwrap_err().to_string();
        assert!(
            err.contains("server.abuse.signup.per_ip.window_secs"),
            "{err}"
        );
    }

    #[test]
    fn abuse_section_deserializes_partial_overrides_over_the_posture() {
        // A config that names only the tracking section: the named surface
        // takes the given values, everything else keeps the shipping
        // posture. (Committed-config parsing end to end is covered by the
        // apps/api integration suite, which loads base.toml through the
        // same figment path as the binary.)
        let config: AbuseControlsConfig = serde_json::from_str(
            r#"{
                "tracking": {
                    "enforce": false,
                    "per_ip": { "limit": 10, "window_secs": 30 }
                }
            }"#,
        )
        .unwrap();
        assert!(!config.tracking.enforce);
        assert_eq!(
            config.tracking.per_ip,
            Some(WindowQuota {
                limit: 10,
                window_secs: 30
            })
        );
        assert!(config.invitation.per_tenant.is_some());
        config.validate().unwrap();
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
    fn transport_defaults_are_the_tuned_values() {
        let config = ServerConfig::default();
        assert_eq!(config.keep_alive_secs, 60);
        assert!(config.compression.enabled);
        assert_eq!(config.compression.min_bytes, 1024);
        config.validate().unwrap();
    }

    #[test]
    fn server_config_without_transport_keys_still_parses() {
        // Configs written before step 7 carry no `keep_alive_secs` or
        // `compression` keys: they take the tuned defaults rather than
        // failing to boot.
        let config: ServerConfig = serde_json::from_value(serde_json::json!({
            "host": "0.0.0.0",
            "port": 8080,
            "workers": 0,
            "shutdown_timeout_secs": 30,
            "request_timeout_secs": 30,
            "max_body_bytes": 1048576,
            "max_concurrency": 10000,
            "cors": {"allowed_origins": [], "allow_credentials": true, "max_age_secs": 600},
            "rate_limit": {"enabled": false, "burst": 60, "per_second": 10, "fail_open": true, "exempt_paths": []},
            "tenant": {"public_prefixes": [], "suspended_allowlist": [], "tenant_rate_limit": null}
        }))
        .expect("pre-step-7 configs parse");
        assert_eq!(config.keep_alive_secs, 60);
        assert!(config.compression.enabled);
        assert_eq!(config.compression.min_bytes, 1024);
        config.validate().unwrap();
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
