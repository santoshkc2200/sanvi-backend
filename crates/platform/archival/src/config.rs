//! The `[archival]` configuration section (shared by the CLI and the
//! worker composition roots).

use serde::{Deserialize, Serialize};

/// Cold-storage configuration. The volume is the local encrypted-volume
/// root (the parked production adapter swaps in object storage behind
/// the same `ColdStorage` port).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ArchivalConfig {
    /// Root directory of the local encrypted volume.
    pub volume: String,
}

impl Default for ArchivalConfig {
    fn default() -> Self {
        Self {
            volume: "./var/archival".to_string(),
        }
    }
}
