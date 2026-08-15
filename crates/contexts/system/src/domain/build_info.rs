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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_info_is_plain_data() {
        let info = BuildInfo::new(
            "0.1.0".into(),
            "abc123".into(),
            "2026-08-15T09:00:00Z".into(),
            "1.93.1".into(),
            "sanvi-api".into(),
        );
        assert_eq!(info.version, "0.1.0");
        assert_eq!(info.commit, "abc123");
        assert_eq!(info.service_name, "sanvi-api");
    }
}
