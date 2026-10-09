//! Domain: pure vocabulary for the system context.
//!
//! Nothing here may import `sqlx`, `actix`, `reqwest` or any other
//! infrastructure crate — enforced by `scripts/check-layering.sh` in CI.

pub mod build_info;
pub mod error;
pub mod health;
pub mod ports;

pub use build_info::{BuildDetails, BuildInfo};
pub use error::SystemError;
pub use health::{DependencyState, DependencyStatus, HealthState, Readiness, ReadinessStates};
pub use ports::HealthProbe;
