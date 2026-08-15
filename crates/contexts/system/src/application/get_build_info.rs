use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::BuildInfo;

/// Query: return build/version information for `/api/v1/system/version`.
pub struct GetBuildInfo {
    info: BuildInfo,
}

impl GetBuildInfo {
    pub fn new(info: BuildInfo) -> Self {
        Self { info }
    }

    pub fn execute(&self) -> BuildInfo {
        self.info.clone()
    }
}

/// Marker DTO (query response is the build info itself).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VersionResponse {
    #[serde(flatten)]
    pub build: BuildInfo,
}
