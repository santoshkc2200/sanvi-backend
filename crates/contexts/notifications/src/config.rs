use serde::Deserialize;

fn default_max_event_age_secs() -> u64 {
    86_400
}

/// Standalone notification-service connection. All fields are deployment
/// secrets / endpoints — set from env (`SANVI__NOTIFICATION__BASE_URL`,
/// `SANVI__NOTIFICATION__SEND_API_KEY`, `SANVI__NOTIFICATION__READ_API_KEY`),
/// never committed. An empty `base_url` is a valid (if useless) default so the
/// process still boots in dev without the service configured — every call then
/// fails closed with `NotificationError::Unavailable`.
#[derive(Clone, Deserialize)]
pub struct NotificationConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub send_api_key: String,
    #[serde(default)]
    pub read_api_key: String,
    #[serde(default = "default_max_event_age_secs")]
    pub max_event_age_secs: u64,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            send_api_key: String::new(),
            read_api_key: String::new(),
            max_event_age_secs: default_max_event_age_secs(),
        }
    }
}

impl std::fmt::Debug for NotificationConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationConfig")
            .field("base_url", &self.base_url)
            .field("send_api_key", &"[REDACTED]")
            .field("read_api_key", &"[REDACTED]")
            .field("max_event_age_secs", &self.max_event_age_secs)
            .finish()
    }
}

impl serde::Serialize for NotificationConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("NotificationConfig", 4)?;
        state.serialize_field("base_url", &self.base_url)?;
        state.serialize_field("send_api_key", "[REDACTED]")?;
        state.serialize_field("read_api_key", "[REDACTED]")?;
        state.serialize_field("max_event_age_secs", &self.max_event_age_secs)?;
        state.end()
    }
}

impl NotificationConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.base_url.is_empty() {
            if !self.base_url.starts_with("http://") && !self.base_url.starts_with("https://") {
                anyhow::bail!("notification.base_url must be an http(s) URL");
            }
            reqwest::Url::parse(&self.base_url)
                .map_err(|e| anyhow::anyhow!("notification.base_url is not a valid URL: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        NotificationConfig::default()
            .validate()
            .expect("defaults validate");
    }

    #[test]
    fn valid_http_and_https_urls_pass_validation() {
        let http_cfg = NotificationConfig {
            base_url: "http://127.0.0.1:8080".to_string(),
            send_api_key: "secret".to_string(),
            read_api_key: "secret".to_string(),
            max_event_age_secs: 86400,
        };
        http_cfg.validate().expect("http URL validates");

        let https_cfg = NotificationConfig {
            base_url: "https://notifications.example.com".to_string(),
            send_api_key: "secret".to_string(),
            read_api_key: "secret".to_string(),
            max_event_age_secs: 86400,
        };
        https_cfg.validate().expect("https URL validates");
    }

    #[test]
    fn invalid_urls_fail_validation() {
        let ftp_cfg = NotificationConfig {
            base_url: "ftp://notifications.example.com".to_string(),
            send_api_key: "secret".to_string(),
            read_api_key: "secret".to_string(),
            max_event_age_secs: 86400,
        };
        assert!(ftp_cfg.validate().is_err());

        let malformed_cfg = NotificationConfig {
            base_url: "http://[invalid-ipv6".to_string(),
            send_api_key: "secret".to_string(),
            read_api_key: "secret".to_string(),
            max_event_age_secs: 86400,
        };
        assert!(malformed_cfg.validate().is_err());
    }

    #[test]
    fn debug_redacts_api_keys() {
        let config = NotificationConfig {
            base_url: "http://localhost:8080".to_string(),
            send_api_key: "super-secret-send-key".to_string(),
            read_api_key: "super-secret-read-key".to_string(),
            max_event_age_secs: 86400,
        };
        let debug_str = format!("{config:?}");
        assert!(!debug_str.contains("super-secret-send-key"));
        assert!(!debug_str.contains("super-secret-read-key"));
        assert!(debug_str.contains("[REDACTED]"));
        assert!(debug_str.contains("http://localhost:8080"));
    }

    #[test]
    fn serialize_redacts_api_keys() {
        let config = NotificationConfig {
            base_url: "http://localhost:8080".to_string(),
            send_api_key: "super-secret-send-key".to_string(),
            read_api_key: "super-secret-read-key".to_string(),
            max_event_age_secs: 86400,
        };
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains("super-secret-send-key"));
        assert!(!serialized.contains("super-secret-read-key"));
        assert!(serialized.contains("[REDACTED]"));
        assert!(serialized.contains("http://localhost:8080"));
    }
}
