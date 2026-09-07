use serde::{Deserialize, Serialize};

/// `hitox-media-service` connection. Both fields are deployment secrets/
/// endpoints — set from env (`SANVI__MEDIA__BASE_URL`,
/// `SANVI__MEDIA__SERVICE_API_KEY`), never committed. An empty
/// `service_api_key` is a valid (if useless) default so the process still
/// boots in dev without the service configured — every call then fails
/// closed with `MediaError::Unavailable`, never silently unauthenticated.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub service_api_key: String,
}
