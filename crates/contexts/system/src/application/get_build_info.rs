use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::{BuildDetails, BuildInfo};

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

/// Query: return the release stamp for `/api/v1/system/build` (FR-1102) —
/// unauthenticated, cacheable, carrying no tenant data.
pub struct GetBuildDetails {
    details: BuildDetails,
}

impl GetBuildDetails {
    /// Derive from the same [`BuildInfo`] the version endpoint serves, so
    /// both endpoints always name the same build.
    pub fn new(build_info: &BuildInfo, environment: String) -> Self {
        Self {
            details: BuildDetails::from_build_info(build_info, environment),
        }
    }

    pub fn execute(&self) -> BuildDetails {
        self.details.clone()
    }
}
