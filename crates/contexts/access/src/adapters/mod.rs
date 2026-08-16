//! Adapters: infrastructure (driven) and transport (driving).

pub mod decision_cache;
pub mod http;
pub mod keto;
pub mod membership_source;
pub mod role_catalog;
pub mod roles_repo;
pub mod sync;
pub mod system;

pub use decision_cache::{InMemoryDecisionCache, NoopDecisionCache};
pub use keto::{HttpKetoClient, KetoCheckError};
pub use membership_source::SqlxMembershipSource;
pub use role_catalog::SqlxRoleCatalog;
pub use roles_repo::SqlxRoleRepository;
pub use system::{SystemClock, SystemRoleIdGenerator};
