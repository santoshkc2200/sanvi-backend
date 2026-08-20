//! The `payments` context: tenant payment providers (phase 09).
//!
//! The invariant that governs every slice of this phase: **Sanvi is never
//! the merchant of record.** Money moves by direct charges on the tenant's
//! own connected account (Stripe Connect, Accounts v2), with
//! `fees_collector` and `losses_collector` both `"stripe"` — funds never
//! touch the platform balance. `scripts/check-connect-invariants.sh` fails
//! CI on the deprecated v1 account-creation and capability-field patterns
//! so a regression fails the build rather than an audit.
//!
//! Sub-phase 09.0 ships only the skeleton: the context compiles, owns a
//! schema with RLS, and is reachable through an entitlement-gated route
//! that returns an empty provider catalog. The domain model, ports and the
//! real catalog arrive with 09.1; Stripe calls with 09.2+. No Stripe SDK
//! call exists yet — that is this slice's exit criteria, not an omission.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::PaymentsConfig;

pub use domain::{
    ConnectionStatus, Currency, Money, PaymentsError, ProviderKind, WebhookVerificationError,
};

/// Wire payments routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::routes::payments_tenant_scope_routes;
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::routes::PERMISSIONED_ROUTES;
}
