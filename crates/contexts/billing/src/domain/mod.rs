//! Domain: entities, value objects, events, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod error;
pub mod events;
pub mod invoice;
pub mod plan;
pub mod ports;
pub mod subscription;

pub use error::BillingError;
pub use events::{BillingEvent, BillingEventName};
pub use invoice::{InvoiceId, InvoiceRef, InvoiceStatus};
pub use plan::{
    EntitlementGrant, Plan, PlanId, PlanPrice, PlanTier, PlanVisibility, PriceId, PriceInterval,
};
pub use ports::{
    CheckoutSessionRequest, CheckoutSessionResult, CheckoutState, CheckoutStateRepository, Clock,
    EntitlementSyncer, EventPublisher, IdGenerator, InvoiceRepository, PlanEntitlementRepository,
    PlanPriceRepository, PlanRepository, PortalSessionRequest, PortalSessionResult, StripeClient,
    StripeCustomer, StripeInvoiceData, StripeSubscriptionData, StripeSubscriptionView,
    SubscriptionRepository, TaxGate, TaxRegistration, TenantLifecycle, WebhookEventRecord,
    WebhookEventRepository, period_from_range,
};
pub use subscription::{
    CollectionState, Period, Subscription, SubscriptionId, SubscriptionSource, SubscriptionStatus,
};
