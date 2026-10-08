//! Resilience primitives shared by every outbound adapter and the HTTP
//! layer (TASK-023): the circuit breaker, the degradation registry behind
//! the `platform.degraded_mode` header, and the job retry budget.
//!
//! Nothing here is flag-gated (NFR-1105): a timeout or a breaker is never
//! the unsafe default, so reverting one is a code revert with a reason in
//! the PR, not a config flip.

pub mod breaker;
pub mod degraded;
pub mod retry_budget;

pub use breaker::{BreakerConfig, BreakerOpen, CircuitBreaker, BreakerState};
pub use degraded::{DegradationReason, DegradationRegistry};
pub use retry_budget::{JobRetryBudget, JobRetryPolicy, RetryOutcome};
