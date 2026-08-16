//! Domain: pure vocabulary for the identity context.
//!
//! Nothing here may import `sqlx`, `actix`, `reqwest` or any other
//! infrastructure crate — enforced by `scripts/check-layering.sh` in CI.
//! The `ScopedTx`/`PlatformScope` capability tokens referenced by ports are
//! opaque handles from the persistence crate, not database access.

pub mod error;
pub mod events;
pub mod invitation;
pub mod link_intent;
pub mod membership;
pub mod ports;
pub mod user;
pub mod values;

pub use error::IdentityError;
pub use events::{IdentityEvent, IdentityEventName};
pub use invitation::{Invitation, InvitationId, InvitationToken};
pub use link_intent::{LinkIntentPayload, LinkNonce};
pub use membership::{Membership, MembershipId, MembershipStatus};
pub use ports::{
    Clock, EventPublisher, IdGenerator, InvitationRepository, KratosClient, LinkIntentRepository,
    LinkedAccountRepository, MembershipRepository, MembershipSource, Notifications, RoleCatalog,
    SessionCache, SessionInfo, UserRepository,
};
pub use user::{LinkedAccount, User, UserId, UserStatus};
pub use values::{Email, Locale, ProviderId, ProviderSubject};
