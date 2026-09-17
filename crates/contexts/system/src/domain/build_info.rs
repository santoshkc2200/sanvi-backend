use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Immutable build/version information, captured at compile time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BuildInfo {
    /// Semantic version of the running binary.
    #[schema(example = "0.1.0")]
    pub version: String,
    /// Git commit the binary was built from (set by the build pipeline).
    #[schema(example = "3d0a5c1e")]
    pub commit: String,
    /// RFC 3339 build timestamp (UTC).
    #[schema(example = "2026-08-15T09:00:00Z")]
    pub built_at: String,
    /// Rust toolchain the binary was compiled with.
    #[schema(example = "1.93.1")]
    pub rust_version: String,
    /// OTel service name of this deployment unit.
    #[schema(example = "sanvi-api")]
    pub service_name: String,
}

impl BuildInfo {
    pub fn new(
        version: String,
        commit: String,
        built_at: String,
        rust_version: String,
        service_name: String,
    ) -> Self {
        Self {
            version,
            commit,
            built_at,
            rust_version,
            service_name,
        }
    }
}

/// Release stamp for field measurements (`GET /api/v1/system/build`,
/// FR-1102): the exact build that served a response, so a latency sample
/// from RUM, the error tracker, or a status page can always be attributed.
/// Carries no tenant data and nothing but the four stamp fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BuildDetails {
    /// Git commit the binary was built from.
    #[schema(example = "3d0a5c1e")]
    pub commit: String,
    /// Semantic version of the running binary.
    #[schema(example = "0.1.0")]
    pub version: String,
    /// RFC 3339 build timestamp (UTC).
    #[schema(example = "2026-08-15T09:00:00Z")]
    pub built_at: String,
    /// Deployment environment the process was configured for
    /// (`local`, `staging`, `production`).
    #[schema(example = "staging")]
    pub environment: String,
}

impl BuildDetails {
    /// Derive the stamp from the same [`BuildInfo`] the version endpoint
    /// serves, so the two can never disagree about the build.
    pub fn from_build_info(build_info: &BuildInfo, environment: String) -> Self {
        Self {
            commit: build_info.commit.clone(),
            version: build_info.version.clone(),
            built_at: build_info.built_at.clone(),
            environment,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> BuildInfo {
        BuildInfo::new(
            "0.1.0".into(),
            "abc123".into(),
            "2026-08-15T09:00:00Z".into(),
            "1.93.1".into(),
            "sanvi-api".into(),
        )
    }

    #[test]
    fn build_info_is_plain_data() {
        let info = sample_info();
        assert_eq!(info.version, "0.1.0");
        assert_eq!(info.commit, "abc123");
        assert_eq!(info.service_name, "sanvi-api");
    }

    #[test]
    fn build_details_derive_from_the_same_build_info() {
        let info = sample_info();
        let details = BuildDetails::from_build_info(&info, "staging".into());
        assert_eq!(details.commit, "abc123");
        assert_eq!(details.version, "0.1.0");
        assert_eq!(details.built_at, "2026-08-15T09:00:00Z");
        assert_eq!(details.environment, "staging");
    }

    /// FR-1102: exactly the four stamp fields — no tenant-scoped member may
    /// ever join this response.
    #[test]
    fn build_details_carry_nothing_beyond_the_stamp() {
        let details = BuildDetails::from_build_info(&sample_info(), "local".into());
        let json = serde_json::to_value(&details).unwrap();
        let mut keys = json
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, vec!["built_at", "commit", "environment", "version"]);
    }
}
