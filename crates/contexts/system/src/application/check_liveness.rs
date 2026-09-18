use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Liveness: is this process alive? (Always yes when the handler runs.)
pub struct CheckLiveness;

impl CheckLiveness {
    pub fn new() -> Self {
        Self
    }

    #[tracing::instrument(name = "system.check_liveness", skip_all)]
    pub fn execute(&self) -> LivenessResult {
        LivenessResult {
            status: "ok".to_string(),
        }
    }
}

impl Default for CheckLiveness {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LivenessResult {
    #[schema(example = "ok")]
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liveness_is_always_ok() {
        assert_eq!(CheckLiveness::new().execute().status, "ok");
    }
}
