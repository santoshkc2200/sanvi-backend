use std::fmt;

use serde::{Deserialize, Serialize, ser::SerializeStruct};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RedisConfig {
    /// May embed credentials (`redis://user:pw@host:port`); never logged raw.
    pub url: String,
    /// Bound on establishing the connection. `ConnectionManager` defaults to
    /// 1s; kept explicit here instead of inheriting the library default.
    pub connect_timeout_ms: u64,
    /// Bound on a single command round-trip. `ConnectionManager` defaults to
    /// 500ms; kept explicit so a slow Redis degrades predictably rather than
    /// silently adopting whatever the library ships next.
    pub response_timeout_ms: u64,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".to_string(),
            connect_timeout_ms: 1_000,
            response_timeout_ms: 500,
        }
    }
}

impl RedisConfig {
    /// Fail fast with *all* configuration problems at once.
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut problems: Vec<String> = Vec::new();
        if let Err(err) = redis::Client::open(self.url.as_str()) {
            problems.push(format!("redis.url is not a valid redis URL: {err}"));
        }
        if self.connect_timeout_ms == 0 {
            problems.push("redis.connect_timeout_ms must be >= 1".to_string());
        }
        if self.response_timeout_ms == 0 {
            problems.push("redis.response_timeout_ms must be >= 1".to_string());
        }
        if !problems.is_empty() {
            anyhow::bail!(
                "invalid redis configuration:\n  - {}",
                problems.join("\n  - ")
            );
        }
        Ok(())
    }

    pub fn connect_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.connect_timeout_ms)
    }

    pub fn response_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.response_timeout_ms)
    }
}

/// The URL may embed a password; never print it via `Debug`.
impl fmt::Debug for RedisConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisConfig")
            .field("url", &redacted_url(&self.url))
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("response_timeout_ms", &self.response_timeout_ms)
            .finish()
    }
}

/// The URL may embed a password; `Serialize` redacts it too, so a future
/// `serde_json::to_string(&app_config)` (e.g. a debug endpoint) can't leak
/// it the way a plain `#[derive(Serialize)]` would.
impl Serialize for RedisConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("RedisConfig", 3)?;
        state.serialize_field("url", &redacted_url(&self.url))?;
        state.serialize_field("connect_timeout_ms", &self.connect_timeout_ms)?;
        state.serialize_field("response_timeout_ms", &self.response_timeout_ms)?;
        state.end()
    }
}

/// Keep scheme/host/port/path visible for debugging; hide userinfo.
fn redacted_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_userinfo, host_and_path)) => format!("{scheme}://<redacted>@{host_and_path}"),
            None => url.to_string(),
        },
        None => "<redacted>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        RedisConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_malformed_url() {
        let config = RedisConfig {
            url: "not a url".to_string(),
            ..RedisConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_auth_in_url() {
        let config = RedisConfig {
            url: "redis://user:pw@localhost:6379/1".to_string(),
            ..RedisConfig::default()
        };
        config.validate().unwrap();
    }

    #[test]
    fn rejects_zero_timeouts() {
        let config = RedisConfig {
            connect_timeout_ms: 0,
            response_timeout_ms: 0,
            ..RedisConfig::default()
        };
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("connect_timeout_ms"));
        assert!(err.contains("response_timeout_ms"));
    }

    #[test]
    fn debug_output_never_contains_credentials() {
        let config = RedisConfig {
            url: "redis://user:hunter2-secret@localhost:6379/1".to_string(),
            ..RedisConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2-secret"));
        assert!(!debug.contains("user:"));
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("localhost:6379"));
    }

    #[test]
    fn debug_output_is_unchanged_when_url_has_no_credentials() {
        let config = RedisConfig::default();
        let debug = format!("{config:?}");
        assert!(debug.contains("redis://localhost:6379"));
    }

    #[test]
    fn serialized_output_never_contains_credentials() {
        let config = RedisConfig {
            url: "redis://user:hunter2-secret@localhost:6379/1".to_string(),
            ..RedisConfig::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("hunter2-secret"));
        assert!(!json.contains("user:"));
        assert!(json.contains("<redacted>"));
        assert!(json.contains("localhost:6379"));
    }
}
