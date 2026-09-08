use serde::Deserialize;

/// `hitox-media-service` connection. Both fields are deployment secrets/
/// endpoints — set from env (`SANVI__MEDIA__BASE_URL`,
/// `SANVI__MEDIA__SERVICE_API_KEY`), never committed. An empty
/// `service_api_key` is a valid (if useless) default so the process still
/// boots in dev without the service configured — every call then fails
/// closed with `MediaError::Unavailable`, never silently unauthenticated.
#[derive(Clone, Default, Deserialize)]
pub struct MediaConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub service_api_key: String,
}

impl std::fmt::Debug for MediaConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaConfig")
            .field("base_url", &self.base_url)
            .field("service_api_key", &"[REDACTED]")
            .finish()
    }
}

impl serde::Serialize for MediaConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("MediaConfig", 2)?;
        state.serialize_field("base_url", &self.base_url)?;
        state.serialize_field("service_api_key", "[REDACTED]")?;
        state.end()
    }
}

impl MediaConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.base_url.is_empty() {
            if !self.base_url.starts_with("http://") && !self.base_url.starts_with("https://") {
                anyhow::bail!("media.base_url must be an http(s) URL");
            }
            reqwest::Url::parse(&self.base_url)
                .map_err(|e| anyhow::anyhow!("media.base_url is not a valid URL: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        MediaConfig::default()
            .validate()
            .expect("defaults validate");
    }

    #[test]
    fn valid_http_and_https_urls_pass_validation() {
        let http_cfg = MediaConfig {
            base_url: "http://127.0.0.1:8080".to_string(),
            service_api_key: "secret".to_string(),
        };
        http_cfg.validate().expect("http URL validates");

        let https_cfg = MediaConfig {
            base_url: "https://media.example.com/prefix".to_string(),
            service_api_key: "secret".to_string(),
        };
        https_cfg.validate().expect("https URL validates");
    }

    #[test]
    fn invalid_urls_fail_validation() {
        let ftp_cfg = MediaConfig {
            base_url: "ftp://media.example.com".to_string(),
            service_api_key: "secret".to_string(),
        };
        assert!(ftp_cfg.validate().is_err());

        let malformed_cfg = MediaConfig {
            base_url: "http://[invalid-ipv6".to_string(),
            service_api_key: "secret".to_string(),
        };
        assert!(malformed_cfg.validate().is_err());
    }

    #[test]
    fn debug_redacts_service_api_key() {
        let config = MediaConfig {
            base_url: "http://localhost:8080".to_string(),
            service_api_key: "super-secret-api-key".to_string(),
        };
        let debug_str = format!("{config:?}");
        assert!(!debug_str.contains("super-secret-api-key"));
        assert!(debug_str.contains("[REDACTED]"));
        assert!(debug_str.contains("http://localhost:8080"));
    }

    #[test]
    fn serialize_redacts_service_api_key() {
        let config = MediaConfig {
            base_url: "http://localhost:8080".to_string(),
            service_api_key: "super-secret-api-key".to_string(),
        };
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains("super-secret-api-key"));
        assert!(serialized.contains("[REDACTED]"));
        assert!(serialized.contains("http://localhost:8080"));
    }
}
