use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RedisConfig {
    pub url: String,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".to_string(),
        }
    }
}

impl RedisConfig {
    /// Fail fast with *all* configuration problems at once.
    pub fn validate(&self) -> anyhow::Result<()> {
        let url = redis::Client::open(self.url.as_str())
            .map_err(|err| anyhow::anyhow!("redis.url is not a valid redis URL: {err}"))?;
        let _ = url.get_connection_info();
        Ok(())
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
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_auth_in_url() {
        let config = RedisConfig {
            url: "redis://user:pw@localhost:6379/1".to_string(),
        };
        config.validate().unwrap();
    }
}
