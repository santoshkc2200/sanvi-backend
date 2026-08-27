//! Sanvi shared kernel.
//!
//! Pure building blocks shared by every bounded context: strongly-typed
//! identifiers, time, pagination, the common error vocabulary and the domain
//! event envelope. Nothing in here may depend on infrastructure (no `sqlx`,
//! no `actix`, no `reqwest`).

pub mod clock;
pub mod error;
pub mod events;
pub mod ids;
pub mod pagination;

pub use clock::{Clock, SystemClock};
pub use error::{IdGenerationError, KernelError};
pub use events::EventEnvelope;
pub use ids::{Id, NodeId, RawIdGenerator, SNOWFLAKE_EPOCH_MS, SnowflakeGenerator, SnowflakeId};
pub use pagination::{Page, PageRequest};
