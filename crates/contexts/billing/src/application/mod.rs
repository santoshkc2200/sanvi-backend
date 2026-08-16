//! Application: use cases and services. Nothing here imports HTTP or
//! database crates; ports are the only way out.

pub mod checkout;
pub mod dunning;
pub mod entitlements_sync;
pub mod override_sub;
pub mod plans;
pub mod portal;
pub mod queries;
pub mod reconcile;
pub mod storage;
pub mod tax;
pub mod webhooks;

pub use checkout::{CheckoutSessionView, CreateCheckoutSession, CreateCheckoutSessionCommand};
pub use dunning::DunningSweep;
pub use override_sub::{
    ApplySubscriptionOverride, ApplySubscriptionOverrideCommand, RevokeSubscriptionOverride,
};
pub use plans::{
    AddPlanPrice, AddPlanPriceCommand, CreatePlan, CreatePlanCommand, ListPlans, ListPublicPlans,
    PlanAdminPriceView, PlanAdminView, PlanPriceView, PublicPlanView, SetPlanEntitlements,
    SetPlanEntitlementsCommand, UpdatePlan, UpdatePlanCommand,
};
pub use portal::{CreatePortalSession, PortalSessionView};
pub use queries::{GetSubscription, InvoiceView, ListInvoices, SubscriptionView};
pub use reconcile::{ReconcileSubscriptions, ReconciliationReport};
pub use tax::CheckTaxRegistrations;
pub use webhooks::{DispatchReport, WebhookDispatcher, WebhookReceiver};

/// Transaction-begin failures are storage failures in this context.
pub(crate) fn storage(err: anyhow::Error) -> crate::domain::BillingError {
    crate::domain::BillingError::Storage(err.to_string())
}
