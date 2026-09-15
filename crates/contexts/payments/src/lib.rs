#![allow(
    clippy::too_many_arguments,
    clippy::field_reassign_with_default,
    clippy::collapsible_if,
    clippy::useless_vec,
    clippy::unnecessary_sort_by,
    clippy::map_identity
)]
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
//! Sub-phase 09.1 ships the provider-agnostic whole: domain aggregates
//! and value objects (money, capabilities, requirements, countries), the
//! connection lifecycle state machine, the `PaymentProviderPort` family,
//! the **fake provider adapter** (the proof a second provider is one
//! adapter, and the frontend's counterpart until sandbox credentials are
//! everywhere), the provider registry the catalog is derived from, and
//! the full outbox event vocabulary. Stripe calls arrive with 09.2+.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::PaymentsConfig;

pub use domain::{
    CapabilitySet, CapabilityStatus, Checkout, CheckoutLifecycleStore, CheckoutRepository,
    CheckoutStatus, CheckoutThrottle, ConnectionLifecycleStore, ConnectionStatus, CountryCode,
    Currency, DisputeFilter, DisputePage, DisputeRecord, DisputeRepository, DisputeStatus,
    MerchantOrderRef, Money, PaymentFilter, PaymentPage, PaymentRecord, PaymentRepository,
    PaymentStatus, PaymentTimeline, PaymentsError, PaymentsEvent, PayoutStatus, ProviderConnection,
    ProviderKind, RefundRecord, RefundRepository, RefundStatus, TenantDomainAllowList,
    TimelineEntry, WebhookEventRecord, WebhookEventRepository, WebhookVerificationError,
};

/// Wire payments routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::routes::payments_tenant_scope_routes;
    /// The internal Connect webhook receiver — mounted by the API binary
    /// only, never part of the public contract.
    pub use crate::adapters::http::webhooks::payments_internal_routes;
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::routes::PERMISSIONED_ROUTES;
}
