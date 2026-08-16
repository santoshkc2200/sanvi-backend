//! `billing` — the platform's own revenue: plans, prices, subscriptions,
//! invoices, dunning and the Stripe integration behind them.
//!
//! Stripe is the source of truth for money; the local projection is the
//! source of truth for *access* (entitlements). Webhooks are stored first
//! and processed asynchronously by the worker, deduplicated on
//! `stripe_event_id` and guarded against out-of-order delivery by comparing
//! event `created` timestamps against local `updated_at`. A daily job
//! reconciles local state back to Stripe.
//!
//! Domain: [`Plan`]/[`PlanPrice`] catalog, [`Subscription`] with the dunning
//! [`CollectionState`] machine, [`InvoiceRef`], webhook event records.
//! Application: checkout/portal session creation, plan CRUD, subscription
//! overrides, webhook dispatch, dunning sweep, reconciliation, the Stripe
//! Tax registration check. Adapters: the HTTP Stripe client (pinned API
//! version, idempotency keys, restricted key), sqlx repositories, the
//! entitlement syncer over the access context's ports, the tenant lifecycle
//! bridge, and the HTTP endpoints.
//!
//! Nothing under `src/domain/` imports infrastructure crates — enforced by
//! `scripts/check-layering.sh`.

pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;

pub use config::BillingConfig;
pub use domain::{
    BillingError, CollectionState, EntitlementGrant, InvoiceRef, Plan, PlanPrice, PriceInterval,
    Subscription, SubscriptionStatus,
};

/// Wire billing routes into an actix `ServiceConfig`.
pub mod routes {
    pub use crate::adapters::http::{
        billing_internal_routes, billing_platform_routes, public_routes, tenant_scope_routes,
    };
}

/// The permission requirements of this context's routes (for the
/// "no route without a permission" CI test).
pub mod catalog {
    pub use crate::adapters::http::catalog::ROUTES as PERMISSIONED_ROUTES;
}
