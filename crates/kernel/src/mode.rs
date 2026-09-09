//! Capability execution modes (disabled, sandbox, live).

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Explicit capability mode for external integrations, gateways, and platform adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMode {
    /// Capability is completely disabled (calls fail fast or register nothing).
    #[default]
    Disabled,
    /// In-memory / fixture / simulated behavior for testing and sandboxed environments.
    Sandbox,
    /// Live production integration with external providers and real credentials.
    Live,
}

impl CapabilityMode {
    /// Canonical string identifier.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Sandbox => "sandbox",
            Self::Live => "live",
        }
    }

    /// Whether this capability is in live production mode.
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }

    /// Whether this capability is running against a sandbox or fake adapter.
    pub fn is_sandbox(&self) -> bool {
        matches!(self, Self::Sandbox)
    }

    /// Whether this capability is explicitly disabled.
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }

    /// Parse a case-insensitive string into a [`CapabilityMode`].
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "disabled" => Ok(Self::Disabled),
            "sandbox" => Ok(Self::Sandbox),
            "live" => Ok(Self::Live),
            other => Err(format!(
                "unknown capability mode: {other:?}, expected one of: disabled, sandbox, live"
            )),
        }
    }
}

impl fmt::Display for CapabilityMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CapabilityMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_and_display() {
        assert_eq!(
            CapabilityMode::parse("disabled").unwrap(),
            CapabilityMode::Disabled
        );
        assert_eq!(
            CapabilityMode::parse("sandbox").unwrap(),
            CapabilityMode::Sandbox
        );
        assert_eq!(CapabilityMode::parse("live").unwrap(), CapabilityMode::Live);
        assert_eq!(
            CapabilityMode::parse("SANDBOX").unwrap(),
            CapabilityMode::Sandbox
        );
        assert!(CapabilityMode::parse("invalid").is_err());

        assert_eq!(CapabilityMode::Disabled.to_string(), "disabled");
        assert_eq!(CapabilityMode::Sandbox.to_string(), "sandbox");
        assert_eq!(CapabilityMode::Live.to_string(), "live");
    }
}
