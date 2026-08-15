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
        if !problems.is_empty() {
            anyhow::bail!(
                "invalid server configuration:\n  - {}",
                problems.join("\n  - ")
            );
        }
        Ok(())
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
