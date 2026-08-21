//! Domain: entities, value objects, events, errors, ports.
//! No infrastructure imports allowed here (enforced by scripts/check-layering.sh).

pub mod capability;
pub mod checkout;
pub mod connection;
pub mod error;
pub mod events;
pub mod money;
pub mod payment;
pub mod ports;
pub mod provider;
pub mod webhook;

pub use capability::{CapabilitySet, CapabilityStatus, RequirementSummary};
pub use checkout::{Checkout, CheckoutStatus, MerchantOrderRef};
pub use connection::ProviderConnection;
pub use error::PaymentsError;
pub use events::{EventPublisher, PaymentsEvent, PaymentsEventName};
pub use money::{Currency, Money};
pub use payment::{PaymentFailure, PaymentRecord, PaymentStatus, RefundRecord, RefundStatus};
pub use ports::{
    CheckoutLifecycleStore, CheckoutRepository, CheckoutThrottle, ConnectionLifecycleStore,
    ConnectionNotifier, CreateAccountRequest, CreateCheckoutCommand, PaymentProviderPort,
    PayoutReadPort, PayoutStatus, ProviderAccount, ProviderAccountSession, ProviderAccountStatus,
    ProviderCheckoutSession, ProviderConnectionRepository, ProviderDescriptor, ProviderPayout,
    ProviderRefund, ProviderRefundCommand, TenantDomainAllowList, WebhookEventRepository,
    WebhookVerifierPort,
};
pub use provider::{ConnectionStatus, CountryCode, ProviderKind};
pub use webhook::{
    WebhookEventRecord, WebhookVerificationError, event_account_id, event_created,
    verify_webhook_signature,
};
