//! `identity` — the context that answers "who are you?".
//!
//! Domain: the user aggregate, memberships, invitations, linked accounts,
//! linking intents, domain events and ports. Application: Kratos webhook
//! projection sync, session verification, account-linking challenge flow,
//! member and invitation management. Adapters: sqlx repositories
//! (tenant-scoped where RLS applies), the Kratos REST client, the Redis
//! session cache, the notification stub and HTTP handlers.
//!
//! Kratos stays the source of truth for credentials and sessions; this
//! context keeps the local projection that joins, RLS and soft-delete need.
//! Nothing under `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::IdentityConfig;
pub use domain::user::LinkedVia;
pub use domain::{
    Email, Invitation, InvitationId, LinkIntentPayload, LinkedAccount, Membership,
    MembershipStatus, ProviderId, ProviderSubject, User, UserId, UserStatus,
};

/// Wire identity routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::{identity_routes, internal_routes, tenant_scope_routes};
}

/// The route catalog: every non-public identity route declared with its
/// permission. The "no route without a permission" CI test enumerates this.
pub mod catalog {
    pub use crate::adapters::http::catalog::ROUTES as PERMISSIONED_ROUTES;
}
